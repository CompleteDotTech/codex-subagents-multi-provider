use super::*;
use crate::CodexThread;
use crate::StateDbHandle;
use crate::ThreadManager;
use crate::agent::agent_status_from_event;
use crate::agent::api::AgentControl;
use crate::agent::api::AgentInfo;
use crate::agent::api::AgentInput;
use crate::agent::api::AgentTarget;
use crate::agent::api::SendRequest;
use crate::agent::api::SpawnRequest;
use crate::agent::next_thread_spawn_depth;
use crate::agent::types::AgentMessage;
use crate::agent::types::LiveAgent;
use crate::agent::types::MessageDeliveryMode;
use crate::agent::types::ResolvedMultiAgentV2UsageHints;
use crate::agent::types::SpawnAgentForkMode;
use crate::agent::types::SpawnAgentOptions;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::config::AgentRoleConfig;
use crate::config::Config;
use crate::config::ConfigBuilder;
use crate::context::BaseInstructionsFragment;
use crate::context::ContextualUserFragment;
use crate::context::ManagedDeveloperInstructions;
use crate::context::ModelInstructions;
use crate::context::MultiAgentRoleInstructions;
use crate::context::SubagentNotification;
use crate::context::world_state::IncrementalToolsHint;
use crate::init_state_db;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::session::SessionSettingsUpdate;
use crate::session::step_context::StepContext;
use crate::thread_manager::ForkSnapshot;
use crate::thread_manager::StartThreadOptions;
use crate::tools::handlers::multi_agents_common::thread_spawn_source;
use assert_matches::assert_matches;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::Instructions;
use codex_extension_api::LoadInstructionsFuture;
use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::ThreadInstructionsProvider;
use codex_extension_api::empty_extension_registry;
use codex_features::Feature;
use codex_history::CompactedItem;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::Settings;
use codex_protocol::error::AgentErrorContext;
use codex_protocol::error::CodexErrKind;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::mcp::McpAttribution;
use codex_protocol::mcp::McpAttributionErrorReason;
use codex_protocol::mcp::McpAttributionSource;
use codex_protocol::mcp::McpAttributionStatus;
use codex_protocol::mcp::OPENAI_FORM_EXTENSION_ID;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageRecord;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnEnvironmentRequests;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::WorldStateItem;
use codex_protocol_test_support::ContentItemTestExt;
use codex_thread_store::ArchiveThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::PersistContext;
use codex_thread_store::ThreadStore;
use codex_utils_path_uri::PathUri;
use core_test_support::responses::strip_response_item_ids;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::future::poll_fn;
use std::sync::RwLock;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use toml::Value as TomlValue;

impl LocalAgentControl {
    pub(crate) async fn prepare_spawn_fixture(
        &self,
        requested: &Config,
        source: &SessionSource,
        full_history: bool,
    ) -> CodexResult<Config> {
        let caller = source
            .parent_thread_id()
            .ok_or_else(|| CodexErr::InvalidRequest("fixture needs a real native parent".into()))?;
        let state = self.runtime.upgrade()?;
        let parent = state.get_thread(caller).await?;
        let turn = parent.session.new_default_turn().await;
        let current = parent.session.get_config().await;
        let model = (requested.model != current.model)
            .then_some(requested.model.as_deref())
            .flatten();
        let effort = (requested.model_reasoning_effort != current.model_reasoning_effort)
            .then_some(requested.model_reasoning_effort.clone())
            .flatten();
        let role = source.get_agent_role();
        let step = StepContext::for_test(turn);
        crate::agent::child_config::prepare_agent_spawn_config(
            &parent.session,
            &step,
            crate::agent::child_config::SpawnConfigOptions {
                resolved_provider: None,
                model_provider: None,
                version: if current.multi_agent_version_from_features() == MultiAgentVersion::V2 {
                    crate::agent::child_config::SpawnConfigVersion::V2
                } else {
                    crate::agent::child_config::SpawnConfigVersion::V1
                },
                full_history_fork: full_history,
                role_name: role.as_deref(),
                model,
                reasoning_effort: effort,
            },
        )
        .await
        .map(|prepared| {
            if full_history {
                if let Some(expected) = requested.developer_instructions.as_ref() {
                    assert_eq!(prepared.config.developer_instructions.as_ref(), Some(expected),
                        "child fixture must configure its developer text through the real host parent/role");
                }
                if !requested.multi_agent_v2.preserve_fork_prefix {
                    assert!(!prepared.config.multi_agent_v2.preserve_fork_prefix,
                        "child fixture must configure fork rewriting through the real host parent");
                }
            }
            prepared.config
        })
        .map_err(CodexErr::InvalidRequest)
    }
}

impl LocalAgentControl {
    /// Create a child fixture through the production spawn entry point.
    pub(crate) async fn spawn_agent_with_metadata(
        &self,
        config: Config,
        initial_input: Vec<UserInput>,
        source: SessionSource,
        mut options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        let config = self
            .prepare_spawn_fixture(&config, &source, options.fork_mode.is_some())
            .await?;
        let mut source = source;
        if let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_role, .. }) = &mut source
        {
            *agent_role = config.prepared_child_authorization.as_ref().and_then(
                crate::agent::child_provider_authorization::PreparedChildAuthorization::role_name,
            );
        }
        let caller = source
            .parent_thread_id()
            .expect("child fixture has a parent");
        options.parent_thread_id = Some(caller);
        self.spawn(SpawnRequest {
            caller,
            config,
            input: AgentInput::UserInput(initial_input),
            source,
            options,
        })
        .await
        .map(|(agent, _)| agent)
    }
}

async fn test_config_with_cli_overrides(
    cli_overrides: Vec<(String, TomlValue)>,
) -> (TempDir, Config) {
    test_config_with_host_configuration(cli_overrides, /*host_configuration*/ None).await
}

async fn test_config_with_host_configuration(
    mut cli_overrides: Vec<(String, TomlValue)>,
    host_configuration: Option<&str>,
) -> (TempDir, Config) {
    let home = TempDir::new().expect("create temp dir");
    if let Some(contents) = host_configuration {
        std::fs::write(home.path().join("config.toml"), contents)
            .expect("write host fixture policy");
    }
    if !cli_overrides
        .iter()
        .any(|(key, _)| key == "agents.max_depth")
    {
        cli_overrides.push(("agents.max_depth".into(), TomlValue::Integer(8)));
    }
    cli_overrides.push((
        "model".to_string(),
        TomlValue::String("gpt-5.5".to_string()),
    ));
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .cli_overrides(cli_overrides)
        .build()
        .await
        .expect("load default test config");
    // Reviewer names used by metadata tests are explicit host-owned roles.
    config
        .agent_roles
        .entry("reviewer".into())
        .or_insert(AgentRoleConfig {
            description: Some("Controller review fixture".into()),
            config_file: None,
            nickname_candidates: None,
        });
    (home, config)
}

async fn test_config() -> (TempDir, Config) {
    test_config_with_cli_overrides(Vec::new()).await
}

fn text_input(text: &str) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }]
}

fn captured_op_matches(actual: &(ThreadId, Op), expected: &(ThreadId, Op)) -> bool {
    if actual.0 != expected.0 {
        return false;
    }
    match (&actual.1, &expected.1) {
        (
            Op::InterAgentCommunication {
                communication: actual,
                ..
            },
            Op::InterAgentCommunication {
                communication: expected,
                ..
            },
        ) => actual == expected,
        _ => false,
    }
}

fn rollout_response_item(item: ResponseItem) -> RolloutItem {
    RolloutItem::ResponseItem(item.into())
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant_message(text: &str, phase: Option<MessagePhase>) -> ResponseItem {
    ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            annotations: None,
            logprobs: None,
            text: text.to_string(),
        }],
        phase,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn register_session_root_skips_threads_with_explicit_parent() {
    let control = LocalAgentControl::default();

    control
        .runtime
        .register_session_root(ThreadId::new(), Some(ThreadId::new()));

    assert_eq!(
        control
            .runtime
            .registry
            .agent_id_for_path(&AgentPath::root()),
        None
    );
}

fn spawn_agent_call(call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        status: None,
        encrypted_content: None,
        id: None,
        name: "spawn_agent".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: call_id.to_string(),
        encrypted_function_args: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

struct AgentControlHarness {
    _home: TempDir,
    config: Config,
    state_db: Option<StateDbHandle>,
    manager: ThreadManager,
    control: LocalAgentControl,
}

impl AgentControlHarness {
    async fn new() -> Self {
        let (home, config) = test_config().await;
        Self::new_with_config(home, config).await
    }

    async fn new_with_config(home: TempDir, config: Config) -> Self {
        let state_db = init_state_db(&config).await;
        let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.to_path_buf(),
            std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            state_db.clone(),
        );
        let control = manager.agent_control();
        Self {
            _home: home,
            config,
            state_db,
            manager,
            control,
        }
    }

    async fn start_thread(&self) -> (ThreadId, Arc<CodexThread>) {
        let new_thread = self
            .manager
            .start_thread_with_control_for_tests(
                StartThreadOptions::new(self.config.clone()),
                self.control.clone(),
            )
            .await
            .expect("start thread");
        (new_thread.thread_id, new_thread.thread)
    }

    async fn start_paginated_thread(&self) -> (ThreadId, Arc<CodexThread>) {
        let new_thread = self
            .manager
            .start_thread_with_control_for_tests(
                StartThreadOptions {
                    history_mode: Some(ThreadHistoryMode::Paginated),
                    environments: Some(Vec::new()),
                    ..StartThreadOptions::new(self.config.clone())
                },
                self.control.clone(),
            )
            .await
            .expect("start paginated thread");
        (new_thread.thread_id, new_thread.thread)
    }

    async fn spawn_anonymous_child(
        &self,
        parent_thread_id: ThreadId,
        options: SpawnAgentOptions,
    ) -> ThreadId {
        self.control
            .spawn_agent_with_metadata(
                self.config.clone(),
                text_input("child task"),
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                }),
                options,
            )
            .await
            .expect("child spawn should succeed")
            .thread_id
    }
}

async fn persisted_originator(thread: &CodexThread) -> String {
    thread.ensure_rollout_materialized().await;
    thread
        .flush_rollout()
        .await
        .expect("thread rollout should flush");
    let stored_thread = thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ false,
        )
        .await
        .expect("thread should be readable");
    stored_thread
        .originator
        .expect("originator should be persisted")
}

fn has_subagent_notification<'a>(
    history_items: impl IntoIterator<Item = &'a ResponseItem>,
) -> bool {
    history_items.into_iter().any(|item| {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        if role != "user" {
            return false;
        }
        content.iter().any(|content_item| match content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                SubagentNotification::matches_text(text)
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => false,
        })
    })
}

/// Returns true when any message item contains `needle` in a text span.
fn history_contains_text<'a>(
    history_items: impl IntoIterator<Item = &'a ResponseItem>,
    needle: &str,
) -> bool {
    history_items.into_iter().any(|item| {
        let ResponseItem::Message { content, .. } = item else {
            return false;
        };
        content.iter().any(|content_item| match content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                text.contains(needle)
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => false,
        })
    })
}

async fn wait_for_recorded_user_message(thread: &CodexThread, needle: &str) {
    timeout(Duration::from_secs(5), async {
        loop {
            let event = thread
                .next_event()
                .await
                .expect("event stream should stay open");
            if let EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::UserMessage(item),
                ..
            }) = event.msg
                && item.content.iter().any(
                    |input| matches!(input, UserInput::Text { text, .. } if text.contains(needle)),
                )
            {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for user message recording");
}

fn history_contains_assistant_inter_agent_communication<'a>(
    history_items: impl IntoIterator<Item = &'a ResponseItem>,
    expected: &InterAgentCommunication,
) -> bool {
    history_items.into_iter().any(|item| {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        if role != "assistant" {
            return false;
        }
        content.iter().any(|content_item| match content_item {
            ContentItem::OutputText { text, .. } => {
                serde_json::from_str::<InterAgentCommunication>(text)
                    .ok()
                    .as_ref()
                    == Some(expected)
            }
            ContentItem::InputText { .. }
            | ContentItem::InputImage { .. }
            | ContentItem::InputAudio { .. } => false,
        })
    })
}

async fn wait_for_subagent_notification(parent_thread: &Arc<CodexThread>) -> bool {
    let wait = async {
        loop {
            let history = parent_thread.session.clone_history().await;
            if has_subagent_notification(history.raw_items()) {
                return true;
            }
            sleep(Duration::from_millis(25)).await;
        }
    };
    // CI can take several seconds to schedule the detached completion watcher,
    // especially on slower Windows runners.
    timeout(Duration::from_secs(10), wait).await.is_ok()
}

async fn persist_thread_for_tree_resume(thread: &Arc<CodexThread>, message: &str) {
    // These tests only need a durable resume fixture. Stop the child prompt
    // first so this marker records directly instead of waiting behind an
    // unrelated active turn.
    thread
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
    thread
        .inject_response_items(vec![user_message(message)])
        .await
        .expect("inject thread resume context");
    thread
        .session
        .ensure_rollout_materialized(PersistContext::Standard)
        .await;
    thread
        .session
        .flush_rollout()
        .await
        .expect("test thread rollout should flush");
}

/// Builds a request entirely in process; it does not install a transport or send anything.
/// The destination filtering behavior is covered separately in `client_tests`.
async fn mcp_attribution_in_constructed_request(thread: &CodexThread) -> McpAttribution {
    let turn_context = thread.session.new_default_turn().await;
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let prompt = crate::session::turn::build_prompt(
        thread
            .session
            .clone_history()
            .await
            .for_prompt(&step_context.settings.model_info.input_modalities),
        &step_context,
        thread.session.get_prompt_base_instructions().await,
        thread
            .session
            .current_window_uses_incremental_tools(&step_context)
            .await,
    );
    let metadata = thread
        .session
        .responses_metadata(&step_context, CodexResponsesRequestKind::Turn)
        .await;
    let request = thread
        .session
        .services
        .model_client
        .build_responses_request(
            &prompt,
            &step_context.settings.model_info,
            /*effort*/ None,
            ReasoningSummary::None,
            /*service_tier*/ None,
            &metadata,
            /*include_internal*/ true,
        )
        .expect("construct request");
    serde_json::from_str(
        request
            .client_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("mcp_attribution"))
            .expect("request MCP attribution"),
    )
    .expect("valid request MCP attribution")
}

async fn wait_for_live_thread_spawn_children(
    control: &LocalAgentControl,
    parent_thread_id: ThreadId,
    expected_children: &[ThreadId],
) {
    let mut expected_children = expected_children.to_vec();
    expected_children.sort_by_key(std::string::ToString::to_string);

    timeout(Duration::from_secs(5), async {
        loop {
            let mut child_ids = control
                .runtime
                .open_thread_spawn_children(parent_thread_id)
                .await
                .expect("live child list should load")
                .into_iter()
                .map(|(thread_id, _)| thread_id)
                .collect::<Vec<_>>();
            child_ids.sort_by_key(std::string::ToString::to_string);
            if child_ids == expected_children {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("expected persisted child tree");
}

async fn assert_thread_not_loaded(manager: &ThreadManager, thread_id: ThreadId) {
    match manager.get_thread(thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(id) => assert_eq!(*id, thread_id),
            _ => panic!("expected ThreadNotFound, got {err:?}"),
        },
        Ok(_) => panic!("expected thread not to be loaded"),
    }
}

#[tokio::test]
async fn send_input_errors_when_manager_dropped() {
    let control = LocalAgentControl::default();
    let err = control
        .send_input(
            ThreadId::new(),
            vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
            Default::default(),
        )
        .await
        .expect_err("send_input should fail without a manager");
    assert_eq!(
        err.to_string(),
        "unsupported operation: thread manager dropped"
    );
}

#[tokio::test]
async fn get_status_returns_not_found_without_manager() {
    let control = LocalAgentControl::default();
    let got = control.get_status(ThreadId::new()).await;
    assert_eq!(got, AgentStatus::NotFound);
}

#[tokio::test]
async fn on_event_updates_status_from_task_started() {
    let status = agent_status_from_event(&EventMsg::TurnStarted(TurnStartedEvent {
        turn_attribution: None,
        turn_id: "turn-1".to_string(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: ModeKind::Default,
    }));
    assert_eq!(status, Some(AgentStatus::Running));
}

#[tokio::test]
async fn on_event_updates_status_from_task_complete() {
    for (error, expected) in [
        (None, AgentStatus::Completed(Some("done".to_string()))),
        (
            Some(ErrorEvent {
                misalignment: None,
                message: "denied".to_string(),
                codex_error_info: None,
            }),
            AgentStatus::Errored("denied".to_string()),
        ),
    ] {
        let status = agent_status_from_event(&EventMsg::TurnComplete(TurnCompleteEvent {
            root_turn_id: None,
            turn_id: "turn-1".to_string(),
            started_at: None,
            last_agent_message: Some("done".to_string()),
            error,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }));
        assert_eq!(status, Some(expected));
    }
}

#[tokio::test]
async fn on_event_updates_status_from_error() {
    let status = agent_status_from_event(&EventMsg::Error(ErrorEvent {
        misalignment: None,
        message: "boom".to_string(),
        codex_error_info: None,
    }));

    let expected = AgentStatus::Errored("boom".to_string());
    assert_eq!(status, Some(expected));
}

#[tokio::test]
async fn on_event_updates_status_from_turn_aborted() {
    let status = agent_status_from_event(&EventMsg::TurnAborted(TurnAbortedEvent {
        root_turn_id: None,
        turn_id: Some("turn-1".to_string()),
        started_at: None,
        reason: TurnAbortReason::Interrupted,
        error: None,
        completed_at: None,
        duration_ms: None,
    }));

    let expected = AgentStatus::Interrupted;
    assert_eq!(status, Some(expected));
}

#[tokio::test]
async fn on_event_updates_status_from_shutdown_complete() {
    let status = agent_status_from_event(&EventMsg::ShutdownComplete);
    assert_eq!(status, Some(AgentStatus::Shutdown));
}

#[tokio::test]
async fn spawn_failure_context_distinguishes_execution_and_residency_capacity() {
    let mut harness = AgentControlHarness::new().await;
    harness
        .config
        .multi_agent_v2
        .max_concurrent_threads_per_session = 2;
    harness
        .control
        .runtime
        .agent_execution_limiter
        .initialize(/*max_threads*/ 1);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
    let running = harness
        .control
        .execution_guard(MultiAgentVersion::V2, &source);
    let execution_error = harness
        .control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect_err("running turn fills execution capacity");
    drop(running);
    harness
        .control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("execution capacity released");

    let state = harness.control.runtime.upgrade().unwrap();
    let membership = harness.control.runtime.admit_start().unwrap();
    let _resident = harness
        .control
        .reserve_v2_residency_slot(
            &state,
            &harness.config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .expect("first pending resident fits");
    let residency_error = match harness
        .control
        .reserve_v2_residency_slot(
            &state,
            &harness.config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
    {
        Ok(_) => panic!("pending resident fills residency capacity"),
        Err(err) => err,
    };
    assert_eq!(
        [execution_error, residency_error].map(|err| (
            err.agent_context(),
            CodexErrKind::from(&err),
            err.to_string(),
        )),
        [
            (
                Some(AgentErrorContext::ExecutionCapacity),
                CodexErrKind::AgentLimitReached,
                "agent thread limit reached".to_string()
            ),
            (
                Some(AgentErrorContext::ResidencyCapacity),
                CodexErrKind::AgentLimitReached,
                "agent thread limit reached".to_string()
            ),
        ],
    );
}

#[tokio::test]
async fn spawn_failure_context_survives_manager_and_fork_errors() {
    let harness = AgentControlHarness::new().await;
    let manager_error = LocalAgentControl::default()
        .spawn_agent(
            harness.config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect_err("missing manager");
    let (parent_id, _) = harness.start_thread().await;
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let config = harness
        .control
        .prepare_spawn_fixture(&harness.config, &source, /*full_history*/ true)
        .await
        .expect("prepare genuine child");
    let fork_error = harness
        .control
        .spawn_agent_internal(
            config,
            spawn::SpawnInitialInput::UserInput(text_input("hello")),
            Some(source),
            SpawnAgentOptions {
                parent_thread_id: Some(parent_id),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect_err("fork requires a parent spawn call id");
    assert_eq!(
        [manager_error, fork_error].map(|err| (err.agent_context(), CodexErrKind::from(&err))),
        [
            (
                Some(AgentErrorContext::ManagerUnavailable),
                CodexErrKind::UnsupportedOperation
            ),
            (Some(AgentErrorContext::ForkHistory), CodexErrKind::Fatal),
        ],
    );
}

#[tokio::test]
async fn spawn_failure_context_preserves_shutdown_during_child_startup() {
    let harness = AgentControlHarness::new().await;
    let state = harness.control.runtime.upgrade().expect("live manager");
    let (parent_id, _) = harness.start_thread().await;
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let config = harness
        .control
        .prepare_spawn_fixture(&harness.config, &source, /*full_history*/ false)
        .await
        .expect("prepare genuine child");
    let spawn = harness.control.spawn_agent_internal(
        config,
        spawn::SpawnInitialInput::UserInput(text_input("hello")),
        Some(source),
        SpawnAgentOptions {
            parent_thread_id: Some(parent_id),
            ..Default::default()
        },
    );
    tokio::pin!(spawn);
    let shutdown = {
        // Block proof validation at real parent lookup. Poll synchronously so the guard
        // never crosses an await, including the one hidden inside futures::poll!.
        let _threads = state.threads.write().await;
        assert!(
            spawn
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        harness.control.runtime.request_shutdown()
    };
    let error = spawn
        .await
        .expect_err("child admission must reject shutdown");
    assert_eq!(
        (
            error.agent_context(),
            CodexErrKind::from(&error),
            error.to_string(),
        ),
        (
            Some(AgentErrorContext::RuntimeShutdown),
            CodexErrKind::InvalidRequest,
            "agent runtime is shutting down".to_string(),
        ),
    );
    shutdown
        .wait()
        .await
        .expect("spawn releases its membership");
}

#[test_case::test_case(false; "stopped caller remains registered")]
#[test_case::test_case(true; "same ID equal authority replacement")]
#[tokio::test]
async fn prepared_fresh_spawn_rejects_stopped_or_replaced_caller(replace: bool) {
    use crate::agent::child_provider_authorization::RetainedAgentAuthority;
    let harness = AgentControlHarness::new().await;
    let started = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start actual parent");
    let parent_id = started.thread_id;
    let parent = started.thread;
    persist_thread_for_tree_resume(&parent, "original caller lifetime fixture").await;
    let stored = parent
        .read_thread(/*include_archived*/ true, /*include_history*/ true)
        .await
        .expect("persisted root history");
    let initial = InitialHistory::Resumed(ResumedHistory {
        history_revision: None,
        conversation_id: parent_id,
        history: Arc::new(stored.history.expect("root history").items),
        rollout_path: stored.rollout_path,
    });
    let mut source = thread_spawn_source(
        parent_id,
        &parent.session_source,
        next_thread_spawn_depth(&parent.session_source),
        /*agent_role*/ None,
        Some("stale_preparation_worker".into()),
    )
    .expect("source for original actual parent");
    let prepared = harness
        .control
        .prepare_spawn_fixture(&harness.config, &source, /*full_history*/ false)
        .await
        .expect("prepare against live original caller");
    let authorization = prepared
        .prepared_child_authorization
        .as_ref()
        .expect("actual host preparation");
    if let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_role, .. }) = &mut source {
        *agent_role = authorization.role_name();
    }
    assert_eq!(source.parent_thread_id(), Some(parent_id));
    let original_authority =
        RetainedAgentAuthority::capture(&parent.session, &parent.session_source).await;
    let origins = &parent
        .session
        .services
        .local_agent_runtime
        .child_provider_origins;
    let original_origin = origins.verified(parent_id);
    assert!(
        original_origin.is_some(),
        "real root startup publishes its provider origin"
    );
    parent
        .shutdown_and_wait()
        .await
        .expect("stop original caller");
    assert!(!parent.is_running());
    let registered = harness
        .manager
        .get_thread(parent_id)
        .await
        .expect("stopped caller retained in manager");
    assert!(Arc::ptr_eq(&registered, &parent));
    let stopped_authority =
        RetainedAgentAuthority::capture(&registered.session, &registered.session_source).await;
    assert_eq!(
        stopped_authority.config.as_ref(),
        original_authority.config.as_ref()
    );
    assert_eq!(stopped_authority.settings, original_authority.settings);
    assert!(stopped_authority.scope == original_authority.scope);
    assert_eq!(origins.verified(parent_id), original_origin);

    let replacement = if replace {
        let removed = harness
            .manager
            .remove_thread(&parent_id)
            .await
            .expect("remove stopped original through manager API");
        assert!(Arc::ptr_eq(&removed, &parent));
        let replacement = harness
            .manager
            .start_thread_with_control_for_tests(
                StartThreadOptions {
                    initial_history: initial,
                    session_source: Some(parent.session_source.clone()),
                    history_mode: Some(ThreadHistoryMode::Legacy),
                    ..StartThreadOptions::new(harness.config.clone())
                },
                harness.control.clone(),
            )
            .await
            .expect("resume actual equal-authority root with same ID");
        assert_eq!(replacement.thread_id, parent_id);
        assert!(replacement.thread.is_running());
        assert!(!Arc::ptr_eq(&replacement.thread.session, &parent.session));
        let actual = harness.manager.get_thread(parent_id).await.unwrap();
        assert!(Arc::ptr_eq(&actual, &replacement.thread));
        let replacement_authority =
            RetainedAgentAuthority::capture(&actual.session, &actual.session_source).await;
        assert_eq!(
            replacement_authority.config.as_ref(),
            original_authority.config.as_ref()
        );
        assert_eq!(replacement_authority.settings, original_authority.settings);
        assert!(replacement_authority.scope == original_authority.scope);
        assert_eq!(
            origins.verified(parent_id),
            original_origin,
            "equal provider origin cannot identify the Session lifetime"
        );
        assert_ne!(
            replacement_authority.epoch().generation,
            original_authority.epoch().generation
        );
        assert!(Arc::ptr_eq(
            origins,
            &actual
                .session
                .services
                .local_agent_runtime
                .child_provider_origins
        ));
        Some(replacement.thread)
    } else {
        None
    };
    assert_eq!(harness.manager.list_thread_ids().await, vec![parent_id]);
    let mut created = harness.manager.subscribe_thread_created();
    let outcome = harness
        .control
        .spawn(SpawnRequest {
            caller: parent_id,
            config: prepared,
            input: AgentInput::UserInput(text_input("must never start for a stale caller")),
            source,
            options: SpawnAgentOptions {
                parent_thread_id: Some(parent_id),
                ..Default::default()
            },
        })
        .await;
    // Preserve a clean fixture even when the current implementation admits the
    // stale preparation. The assertion below records that actual unexpected success.
    if let Ok((agent, _)) = &outcome {
        let child = harness
            .manager
            .get_thread(agent.thread_id)
            .await
            .expect("unexpected child is registered");
        child
            .shutdown_and_wait()
            .await
            .expect("clean unexpected stale-authorized child");
        harness
            .manager
            .remove_thread(&agent.thread_id)
            .await
            .expect("remove unexpected child");
    }
    if let Some(replacement) = replacement {
        replacement
            .shutdown_and_wait()
            .await
            .expect("clean replacement caller");
    }
    let error = match outcome {
        Ok(_) => panic!("stale preparation unexpectedly authorized actual child startup"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("child_provider_policy_conflict"),
        "{error}"
    );
    assert!(
        matches!(
            created.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "stale caller rejection must precede child startup publication"
    );
    assert_eq!(harness.manager.list_thread_ids().await, vec![parent_id]);
}

#[tokio::test]
async fn cancelled_spawn_after_input_admission_discards_child() {
    let harness = AgentControlHarness::new().await;
    let state = harness.control.runtime.upgrade().expect("live manager");
    let (parent_id, _parent) = harness.start_thread().await;
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let prepared = harness
        .control
        .prepare_spawn_fixture(&harness.config, &source, /*full_history*/ false)
        .await
        .unwrap();
    let mut spawn = harness.control.spawn(SpawnRequest {
        caller: parent_id,
        config: prepared,
        input: AgentInput::UserInput(text_input("child task")),
        source: SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: parent_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        options: SpawnAgentOptions {
            parent_thread_id: Some(parent_id),
            ..Default::default()
        },
    });
    let child = poll_fn(|cx| {
        assert!(spawn.as_mut().poll(cx).is_pending());
        let threads = state.threads.try_read().expect("uncontended manager");
        // On this single-thread runtime the session loop cannot consume the first
        // input until this poll returns. Leave spawn waiting for its admission reply.
        match threads.values().find(|thread| !thread.io.tx_sub.is_empty()) {
            Some(child) => Poll::Ready(Arc::clone(child)),
            None => Poll::Pending,
        }
    })
    .await;
    let child_id = child.session.thread_id();
    // This no-op goes through the same FIFO submission loop. Its receipt proves the
    // earlier input has been admitted, without polling the spawn future again.
    child
        .update_thread_settings(ThreadSettingsOverrides::default())
        .await
        .expect("input admission completed before the settings barrier");
    {
        // Only the final status lookup now needs the manager lock. Cancel there,
        // after input admission but before the backend can return its receipt.
        let _threads = state.threads.write().await;
        assert!(
            spawn
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(spawn);
    }
    assert!(
        harness.control.get_agent_metadata(child_id).is_none(),
        "cancelled spawn must not leave an accepted child registered"
    );
    child.wait_until_terminated().await;
    assert_thread_not_loaded(&harness.manager, child_id).await;
    harness
        .control
        .runtime
        .request_shutdown()
        .wait()
        .await
        .expect("provisional cleanup releases tree membership");
}

#[tokio::test]
async fn native_spawn_fixture_rejects_nonexistent_parent() {
    let harness = AgentControlHarness::new().await;
    let missing = ThreadId::new();
    let result = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("unowned child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: missing,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
        )
        .await;
    let error = result.expect_err("nonexistent native parent must be rejected");
    assert!(matches!(error.details(), CodexErrorDetails::ThreadNotFound(id) if *id == missing));
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

#[tokio::test]
async fn spawn_agent_errors_when_manager_dropped() {
    let control = LocalAgentControl::default();
    let (_home, config) = test_config().await;
    let err = control
        .spawn_agent(config, text_input("hello"), /*session_source*/ None)
        .await
        .expect_err("spawn_agent should fail without a manager");
    assert_eq!(
        err.to_string(),
        "unsupported operation: thread manager dropped"
    );
}

#[tokio::test]
async fn resume_agent_errors_when_manager_dropped() {
    let control = LocalAgentControl::default();
    let (_home, config) = test_config().await;
    let err = control
        .resume_agent_from_rollout(config, ThreadId::new(), SessionSource::Exec)
        .await
        .expect_err("resume_agent should fail without a manager");
    assert_eq!(
        err.to_string(),
        "unsupported operation: thread manager dropped"
    );
}

#[tokio::test]
async fn send_input_errors_when_thread_missing() {
    let harness = AgentControlHarness::new().await;
    let thread_id = ThreadId::new();
    let err = harness
        .control
        .send_input(
            thread_id,
            vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
            Default::default(),
        )
        .await
        .expect_err("send_input should fail for missing thread");
    assert_matches!(
        err.details(),
        CodexErrorDetails::ThreadNotFound(id) if *id == thread_id
    );
}

#[tokio::test]
async fn get_status_returns_not_found_for_missing_thread() {
    let harness = AgentControlHarness::new().await;
    let status = harness.control.get_status(ThreadId::new()).await;
    assert_eq!(status, AgentStatus::NotFound);
}

#[tokio::test]
async fn get_status_returns_pending_init_for_new_thread() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, _) = harness.start_thread().await;
    let status = harness.control.get_status(thread_id).await;
    assert_eq!(status, AgentStatus::PendingInit);
}

#[tokio::test]
async fn subscribe_status_errors_for_missing_thread() {
    let harness = AgentControlHarness::new().await;
    let thread_id = ThreadId::new();
    let err = harness
        .control
        .subscribe_status(thread_id)
        .await
        .err()
        .expect("subscribe_status should fail for missing thread");
    assert_matches!(
        err.details(),
        CodexErrorDetails::ThreadNotFound(id) if *id == thread_id
    );
}

#[tokio::test]
async fn subscribe_status_updates_on_shutdown() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, thread) = harness.start_thread().await;
    let mut status_rx = harness
        .control
        .subscribe_status(thread_id)
        .await
        .expect("subscribe_status should succeed");
    let _ = thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
    thread.wait_until_terminated().await;

    // The update arrives before the first poll, without replacing the initial snapshot.
    let initial = status_rx
        .next()
        .await
        .expect("initial snapshot")
        .expect("snapshot");
    assert_eq!(initial.status(), Some(&AgentStatus::PendingInit));
    let shutdown = status_rx
        .next()
        .await
        .expect("shutdown snapshot")
        .expect("snapshot");
    assert_eq!(shutdown.status(), Some(&AgentStatus::Shutdown));
}

#[tokio::test]
async fn send_input_submits_user_message() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, thread) = harness.start_thread().await;

    let submission_id = harness
        .control
        .send_input(
            thread_id,
            vec![UserInput::Text {
                text: "hello from tests".to_string(),
                text_elements: Vec::new(),
            }],
            Default::default(),
        )
        .await
        .expect("send_input should succeed");
    assert!(!submission_id.is_empty());
    wait_for_recorded_user_message(thread.as_ref(), "hello from tests").await;
}

#[tokio::test]
async fn send_inter_agent_communication_without_turn_queues_message_without_triggering_turn() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, thread) = harness.start_thread().await;
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "hello from tests".to_string(),
        /*trigger_turn*/ false,
    );

    let submission_id = harness
        .control
        .send_inter_agent_communication(
            thread_id,
            communication.clone(),
            AgentCommunicationContext::new(AgentCommunicationKind::Message, ThreadId::new()),
            Default::default(),
        )
        .await
        .expect("send_inter_agent_communication should succeed");
    assert!(!submission_id.is_empty());

    let expected = (
        thread_id,
        Op::InterAgentCommunication {
            communication: communication.clone(),
            start_options: Default::default(),
        },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| captured_op_matches(entry, &expected));
    assert!(captured.is_some());

    timeout(Duration::from_secs(5), async {
        loop {
            if thread
                .session
                .input_queue
                .has_pending_input(&thread.session.active_turn)
                .await
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("inter-agent communication should stay pending");

    let history = thread.session.clone_history().await;
    assert!(!history_contains_assistant_inter_agent_communication(
        history.raw_items(),
        &communication
    ));
}

#[tokio::test]
async fn ensure_v2_agent_loaded_reloads_registered_unloaded_agent() {
    check_v2_agent_reload(V2ReloadRoute::Sender).await;
}

#[tokio::test]
async fn ensure_v2_child_loaded_preserves_evicted_parent_authority() {
    check_v2_agent_reload(V2ReloadRoute::NestedParent).await;
}

#[tokio::test]
async fn cancelled_v2_send_finishes_reload_registration() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::Sqlite).unwrap();
    // The root leaves exactly one resident child slot available.
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_paginated_thread().await;
    let control = parent
        .session
        .services
        .local_agent_runtime
        .control(parent.session.session_id());
    let child =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &parent, "worker").await;
    let child_id = child.thread_id;
    let thread = harness.manager.get_thread(child_id).await.unwrap();
    thread
        .inject_response_items(vec![assistant_message(
            "child persisted",
            Some(MessagePhase::FinalAnswer),
        )])
        .await
        .unwrap();
    // The existing fixture has no task runner to finish its initial turn.
    thread.session.mark_interrupted();
    *thread.session.active_turn.lock().await = None;
    assert_eq!(
        harness.manager.try_evict_v2_thread(thread).await.unwrap(),
        ThreadEvictionOutcome::Evicted,
    );
    assert!(
        control
            .runtime
            .registry
            .evicted_environments(child_id)
            .is_some()
    );

    let followup_request = |message: &str| SendRequest {
        caller: parent_id,
        target: AgentTarget::Id(child_id),
        resume_config: harness.config.clone(),
        input: AgentInput::Message {
            message: AgentMessage::Plaintext(message.to_string()),
            mode: MessageDeliveryMode::TriggerTurn,
        },
        start_options: Default::default(),
    };
    let mut created = harness.manager.subscribe_thread_created();
    let state = control.runtime.upgrade().unwrap();
    let mut send = control.send(followup_request("cancelled follow-up"));
    let (reloaded, pin_blocker) = poll_fn(|cx| {
        assert!(send.as_mut().poll(cx).is_pending());
        let Ok(threads) = state.threads.try_read() else {
            return Poll::Pending;
        };
        let Some(reloaded) = threads.get(&child_id).cloned() else {
            return Poll::Pending;
        };
        // On this single-thread runtime, intercept publication before the detached
        // registration task can acquire its residency pin.
        let pin_blocker = Arc::clone(&reloaded.residency_gate)
            .try_write_owned()
            .expect("reload registration has not acquired its pin yet");
        Poll::Ready((reloaded, pin_blocker))
    })
    .await;
    tokio::task::yield_now().await;
    assert!(matches!(
        created.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    drop(send);
    drop(pin_blocker);

    assert_eq!(
        timeout(Duration::from_secs(5), created.recv())
            .await
            .expect("cancelled sender must not cancel reload registration")
            .unwrap(),
        child_id,
    );
    assert!(
        control
            .runtime
            .registry
            .evicted_environments(child_id)
            .is_none()
    );
    // Evict and reload again with the single child slot. This also catches a leaked
    // pin or pending residency reservation left behind by the cancelled sender.
    assert_eq!(
        harness.manager.try_evict_v2_thread(reloaded).await.unwrap(),
        ThreadEvictionOutcome::Evicted,
    );
    let receipt = control
        .send(followup_request("follow-up after cancellation"))
        .await
        .expect("follow-up should reload the same child after cancellation");
    assert_eq!(receipt.thread_id, child_id);
    let messages: Vec<_> = harness
        .manager
        .captured_ops()
        .into_iter()
        .filter_map(|(thread_id, op)| match op {
            Op::InterAgentCommunication { communication, .. } if thread_id == child_id => {
                Some(communication)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        messages,
        vec![InterAgentCommunication::new(
            AgentPath::root(),
            child.metadata.agent_path.unwrap(),
            Vec::new(),
            "Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\nfollow-up after cancellation".to_string(),
            /*trigger_turn*/ true,
        )],
    );
}

#[derive(Clone, Copy)]
enum V2ReloadRoute {
    Sender,
    NestedParent,
}

async fn spawn_v2_reload_test_child(
    control: &LocalAgentControl,
    config: Config,
    parent: &CodexThread,
    task_name: &str,
) -> LiveAgent {
    let source = thread_spawn_source(
        parent.session.thread_id,
        &parent.session_source,
        next_thread_spawn_depth(&parent.session_source),
        /*agent_role*/ None,
        Some(task_name.to_string()),
    )
    .expect("child source");
    control
        .spawn_agent_with_metadata(
            config,
            text_input("hello child"),
            source,
            SpawnAgentOptions {
                parent_thread_id: Some(parent.session.thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("spawn_agent should succeed")
}

async fn check_v2_agent_reload(route: V2ReloadRoute) {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    config.model = Some("gpt-5.6-sol".to_string());
    config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    config.permissions.allow_login_shell = true;
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .expect("read-only parent profile");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let client_mcp_extensions =
        ClientMcpExtensions::new([(OPENAI_FORM_EXTENSION_ID.to_string(), serde_json::json!({}))]);
    let root = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Paginated),
                client_mcp_extensions: client_mcp_extensions.clone(),
                user_instructions: Some(LoadedUserInstructions {
                    instructions: Some(Instructions {
                        text: "global instructions survive parent eviction".to_string(),
                        source: None,
                    }),
                    warnings: Vec::new(),
                }),
                thread_instructions_provider: Some(Arc::new(TestThreadInstructionsProvider {
                    text: "thread instructions survive parent eviction".into(),
                    shared: false,
                })),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start root thread");
    let control = root
        .thread
        .session
        .services
        .local_agent_runtime
        .control(root.thread.session.session_id());
    let parent_thread = match route {
        V2ReloadRoute::Sender => root.thread,
        V2ReloadRoute::NestedParent => {
            let parent = spawn_v2_reload_test_child(
                &control,
                harness.config.clone(),
                &root.thread,
                "parent",
            )
            .await;
            harness
                .manager
                .get_thread(parent.thread_id)
                .await
                .expect("nested parent should exist")
        }
    };
    let parent_thread_id = parent_thread.session.thread_id;
    let inherited_instructions = parent_thread.session.inherited_instructions().await;
    assert!(inherited_instructions.user.is_some());
    assert!(inherited_instructions.thread.is_some());
    let mut child_config = harness.config.clone();
    child_config.model = Some("gpt-5.6-luna".to_string());
    let spawned_agent =
        spawn_v2_reload_test_child(&control, child_config, &parent_thread, "worker").await;
    let agent_path = spawned_agent
        .metadata
        .agent_path
        .clone()
        .expect("agent path");
    let child_thread = harness
        .manager
        .get_thread(spawned_agent.thread_id)
        .await
        .expect("child thread should exist");
    child_thread
        .inject_response_items(vec![assistant_message(
            "child persisted",
            Some(MessagePhase::FinalAnswer),
        )])
        .await
        .expect("child rollout should persist with v2 metadata");
    child_thread
        .shutdown_and_wait()
        .await
        .expect("child thread should shut down");
    let stored_child = child_thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ false,
        )
        .await
        .expect("child metadata should be readable");
    assert_eq!(stored_child.history_mode, ThreadHistoryMode::Paginated);

    assert!(
        harness
            .manager
            .remove_thread(&spawned_agent.thread_id)
            .await
            .is_some()
    );
    assert_matches!(
        control
            .inspect_agent(spawned_agent.thread_id)
            .await
            .expect("inspect registered unloaded agent"),
        AgentInfo::Unloaded(_)
    );
    // Inspection must not restore the evicted runtime.
    match harness.manager.get_thread(spawned_agent.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(id) => assert_eq!(*id, spawned_agent.thread_id),
            _ => panic!("expected ThreadNotFound, got {err:?}"),
        },
        Ok(_) => panic!("expected thread to be removed"),
    }

    let mut sender_config = harness.config.clone();
    sender_config.model_provider_id = "ollama".to_string();
    sender_config.model_provider = sender_config
        .model_providers
        .get("ollama")
        .cloned()
        .expect("ollama provider should be configured");

    let mut parent_turn = parent_thread.session.new_default_turn().await;
    let residency_pin = match route {
        V2ReloadRoute::Sender => control
            .ensure_v2_agent_loaded(
                sender_config,
                spawned_agent.thread_id,
                parent_thread_id,
                /*parent*/ None,
            )
            .await
            .expect("known v2 agent should reload"),
        V2ReloadRoute::NestedParent => {
            let environment = parent_turn
                .initial_environments
                .primary()
                .expect("parent environment");
            let mut owner_config = environment.config().clone();
            owner_config.allow_login_shell = false;
            let mut selection = environment.selection();
            selection.config = EnvironmentConfigState::Ready(owner_config);
            parent_thread
                .session
                .update_settings(SessionSettingsUpdate {
                    environments: Some(TurnEnvironmentSelections::new(
                        parent_turn.config.cwd.clone(),
                        vec![selection],
                    )),
                    ..Default::default()
                })
                .await
                .expect("save parent environments");
            parent_thread.session.mark_interrupted();
            // The fixture has no task runner to finish the turn or consume child results.
            *parent_thread.session.active_turn.lock().await = None;
            let _ = parent_thread
                .session
                .input_queue
                .drain_mailbox_input_items()
                .await;
            harness
                .manager
                .ensure_multi_agent_v2_child_loaded(spawned_agent.thread_id)
                .await
                .expect("known child should reload through its parent");
            parent_turn = parent_thread.session.new_default_turn().await;
            assert!(harness.manager.get_thread(parent_thread_id).await.is_err());
            None
        }
    };
    let reloaded_child = harness
        .manager
        .get_thread(spawned_agent.thread_id)
        .await
        .expect("reloaded child thread should exist");
    if matches!(route, V2ReloadRoute::Sender) {
        assert_eq!(
            harness
                .manager
                .try_evict_v2_thread(Arc::clone(&reloaded_child))
                .await
                .expect("attempt eviction before input submission"),
            crate::ThreadEvictionOutcome::Busy,
        );
    }
    let reloaded_instructions = reloaded_child.session.inherited_instructions().await;
    assert_eq!(
        (reloaded_instructions.user, reloaded_instructions.thread),
        (inherited_instructions.user, inherited_instructions.thread),
        "reloading a child must retain both parent snapshots, even if residency evicts the parent",
    );
    if matches!(route, V2ReloadRoute::NestedParent) {
        let reloaded_turn = reloaded_child.session.new_default_turn().await;
        assert_eq!(
            (
                reloaded_turn.initial_environments.to_selections(),
                reloaded_turn.permission_profile(),
                reloaded_child.client_mcp_extensions(),
            ),
            (
                parent_turn.initial_environments.to_selections(),
                parent_turn.permission_profile(),
                client_mcp_extensions,
            ),
        );
        assert!(Arc::ptr_eq(
            &reloaded_child.session.services.exec_policy,
            &parent_thread.session.services.exec_policy,
        ));
    }
    assert_eq!(
        reloaded_child.config_snapshot().await.model,
        "gpt-5.6-luna",
        "residency reload must preserve the worker model instead of inheriting its parent model",
    );
    assert_eq!(
        (
            reloaded_child.config_snapshot().await.model_provider_id,
            reloaded_child
                .session
                .new_default_turn()
                .await
                .provider
                .info()
                .clone(),
        ),
        (
            stored_child.model_provider,
            harness.config.model_provider.clone()
        ),
        "residency reload must preserve the worker provider instead of inheriting its sender's provider",
    );

    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        agent_path,
        Vec::new(),
        "hello after reload".to_string(),
        /*trigger_turn*/ false,
    );
    control
        .send_inter_agent_communication(
            spawned_agent.thread_id,
            communication.clone(),
            AgentCommunicationContext::new(AgentCommunicationKind::Message, ThreadId::new()),
            Default::default(),
        )
        .await
        .expect("send_inter_agent_communication should succeed after reload");
    drop(residency_pin);
    let expected = (
        spawned_agent.thread_id,
        Op::InterAgentCommunication {
            communication,
            start_options: Default::default(),
        },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| captured_op_matches(entry, &expected));
    assert!(captured.is_some());
}

#[tokio::test]
async fn resume_agent_from_rollout_does_not_reopen_v2_descendants() {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let worker_path = AgentPath::root().join("worker").expect("worker path");
    let worker_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello worker"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("worker spawn should succeed");
    let reviewer_path = worker_path.join("reviewer").expect("reviewer path");
    let reviewer_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello reviewer"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: Some(reviewer_path.clone()),
                agent_nickname: None,
                agent_role: Some("reviewer".to_string()),
            })),
        )
        .await
        .expect("reviewer spawn should succeed");
    let sibling_thread_id = harness
        .spawn_anonymous_child(parent_thread_id, SpawnAgentOptions::default())
        .await;

    let worker_thread = harness
        .manager
        .get_thread(worker_thread_id)
        .await
        .expect("worker thread should exist");
    let reviewer_thread = harness
        .manager
        .get_thread(reviewer_thread_id)
        .await
        .expect("reviewer thread should exist");
    let sibling_thread = harness
        .manager
        .get_thread(sibling_thread_id)
        .await
        .expect("sibling thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&worker_thread, "worker persisted").await;
    persist_thread_for_tree_resume(&reviewer_thread, "reviewer persisted").await;
    persist_thread_for_tree_resume(&sibling_thread, "sibling persisted").await;
    wait_for_live_thread_spawn_children(
        &harness.control,
        parent_thread_id,
        &[worker_thread_id, sibling_thread_id],
    )
    .await;
    wait_for_live_thread_spawn_children(&harness.control, worker_thread_id, &[reviewer_thread_id])
        .await;

    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());

    let resumed_manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        harness.config.model_provider.clone(),
        harness.config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        harness.state_db.clone(),
    );
    let resumed_control = resumed_manager.agent_control();
    let resumed_parent_thread_id = resumed_control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
        )
        .await
        .expect("v2 root resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        resumed_control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_thread_not_loaded(&resumed_manager, worker_thread_id).await;
    assert_thread_not_loaded(&resumed_manager, reviewer_thread_id).await;
    assert_thread_not_loaded(&resumed_manager, sibling_thread_id).await;
    resumed_control
        .restore_v2_agent_metadata(&harness.config, parent_thread_id)
        .await;
    for thread_id in [worker_thread_id, sibling_thread_id] {
        assert!(
            resumed_control
                .runtime
                .ensure_agent_known(thread_id)
                .is_ok()
        );
    }

    resumed_control
        .close_agent(worker_thread_id)
        .await
        .expect("closing a restored sibling should succeed");

    let closed_worker = resumed_control.runtime.ensure_agent_known(worker_thread_id);
    let surviving_sibling = resumed_control
        .runtime
        .ensure_agent_known(sibling_thread_id);
    assert!(closed_worker.is_err());
    assert!(surviving_sibling.is_ok());
    assert_thread_not_loaded(&resumed_manager, sibling_thread_id).await;
}

struct TestThreadInstructionsProvider {
    text: RwLock<&'static str>,
    shared: bool,
}

impl ThreadInstructionsProvider for TestThreadInstructionsProvider {
    fn share_with_subagents(&self) -> bool {
        self.shared
    }

    fn load_thread_instructions(&self) -> LoadInstructionsFuture<'_> {
        let instructions = Instructions {
            text: self
                .text
                .read()
                .expect("read thread instructions")
                .to_string(),
            source: None,
        };
        Box::pin(async move {
            LoadedUserInstructions {
                instructions: Some(instructions),
                warnings: Vec::new(),
            }
        })
    }
}

#[test_case::test_case(false, false; "snapshot")]
#[test_case::test_case(true, false; "shared")]
#[test_case::test_case(false, true; "unshared_grandchild_without_loaded_parent")]
#[test_case::test_case(true, true; "shared_grandchild_without_loaded_parent")]
#[tokio::test]
async fn cold_resume_with_thread_instructions_preserves_lazy_v2_child_inheritance(
    shared: bool,
    nested: bool,
) {
    let (home, mut config) = test_config_with_host_configuration(
        Vec::new(),
        Some("[agents]\nallowed_model_providers=['openai']\n"),
    )
    .await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let parent = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                thread_instructions_provider: Some(Arc::new(TestThreadInstructionsProvider {
                    text: "initial thread instructions".into(),
                    shared,
                })),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start parent with thread instructions");
    let parent_thread_id = parent.thread_id;
    let control = &parent
        .thread
        .session
        .services
        .local_agent_runtime
        .control(parent.thread.session.session_id());
    let worker_thread_id =
        spawn_v2_reload_test_child(control, harness.config.clone(), &parent.thread, "worker")
            .await
            .thread_id;
    let worker = harness
        .manager
        .get_thread(worker_thread_id)
        .await
        .expect("worker should be loaded");
    persist_thread_for_tree_resume(&parent.thread, "parent persisted").await;
    persist_thread_for_tree_resume(&worker, "worker persisted").await;
    wait_for_live_thread_spawn_children(control, parent_thread_id, &[worker_thread_id]).await;

    let target_thread_id = if nested {
        let grandchild_id =
            spawn_v2_reload_test_child(control, harness.config.clone(), &worker, "reviewer")
                .await
                .thread_id;
        let grandchild = harness
            .manager
            .get_thread(grandchild_id)
            .await
            .expect("grandchild is loaded");
        persist_thread_for_tree_resume(&grandchild, "grandchild persisted").await;
        wait_for_live_thread_spawn_children(control, worker_thread_id, &[grandchild_id]).await;
        grandchild_id
    } else {
        worker_thread_id
    };

    let stored_parent = parent
        .thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .expect("read parent history");
    let initial_history = InitialHistory::Resumed(ResumedHistory {
        history_revision: None,
        conversation_id: parent_thread_id,
        history: Arc::new(stored_parent.history.expect("parent history").items),
        rollout_path: stored_parent.rollout_path,
    });
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());

    let resumed_manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        harness.config.model_provider.clone(),
        harness.config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        harness.state_db.clone(),
    );
    let provider: Arc<dyn ThreadInstructionsProvider> = Arc::new(TestThreadInstructionsProvider {
        text: "resumed thread instructions".into(),
        shared,
    });
    let resumed_parent = resumed_manager
        .start_thread(StartThreadOptions {
            initial_history,
            session_source: Some(SessionSource::Exec),
            thread_instructions_provider: Some(provider.clone()),
            ..StartThreadOptions::new(harness.config.clone())
        })
        .await
        .expect("cold resume parent with updated thread instructions");
    let thread_instructions = Instructions {
        text: "resumed thread instructions".to_string(),
        source: None,
    };
    assert_eq!(
        resumed_parent
            .thread
            .session
            .inherited_instructions()
            .await
            .thread,
        Some(thread_instructions.clone()),
    );
    assert_thread_not_loaded(&resumed_manager, worker_thread_id).await;

    if nested {
        let turn = resumed_parent.thread.session.new_default_turn().await;
        resumed_parent
            .thread
            .session
            .services
            .agent_control
            .send(crate::SendRequest {
                caller: parent_thread_id,
                target: crate::AgentTarget::Id(target_thread_id),
                resume_config: crate::agent::child_config::build_agent_resume_config(&turn)
                    .expect("capture resume config"),
                input: crate::AgentInput::Message {
                    message: AgentMessage::Plaintext("hello after resume".to_string()),
                    mode: MessageDeliveryMode::QueueOnly,
                },
                start_options: TurnStartOptions {
                    root_turn_id: turn.turn_metadata_state.root_turn_id(),
                    turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
                    cyber_access_program: turn.cyber_access_program,
                    ..Default::default()
                },
            })
            .await
            .expect("message should reload the grandchild");
        assert_thread_not_loaded(&resumed_manager, worker_thread_id).await;
    } else {
        resumed_manager
            .ensure_multi_agent_v2_child_loaded(worker_thread_id)
            .await
            .expect("resume v2 worker on demand");
    }
    let resumed_target = resumed_manager
        .get_thread(target_thread_id)
        .await
        .expect("resumed target should be loaded");
    let inherited = resumed_target.session.inherited_instructions().await;
    let root_provider = resumed_parent
        .thread
        .session
        .inherited_instructions()
        .await
        .thread_provider;
    assert_eq!(
        inherited
            .thread_provider
            .as_ref()
            .map(|inherited| root_provider
                .as_ref()
                .is_some_and(|root| Arc::ptr_eq(inherited, root))),
        shared.then_some(true),
    );
    assert_eq!(
        inherited.thread,
        (!nested || shared).then_some(thread_instructions),
    );
}

enum InstructionsReloadTarget {
    Sibling,
    Root,
}

#[test_case::test_case(false, InstructionsReloadTarget::Sibling; "snapshot_is_not_shared")]
#[test_case::test_case(true, InstructionsReloadTarget::Sibling; "shared_provider_survives")]
#[test_case::test_case(true, InstructionsReloadTarget::Root; "root_reuses_surviving_shared_provider")]
#[tokio::test]
async fn v2_reload_preserves_shared_instructions_after_root_unloads(
    shared: bool,
    reload_target: InstructionsReloadTarget,
) {
    let (home, mut config) = test_config().await;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("enable multi-agent v2");
    config
        .features
        .enable(Feature::Sqlite)
        .expect("enable SQLite");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let provider = Arc::new(TestThreadInstructionsProvider {
        text: "initial thread instructions".into(),
        shared,
    });
    let root = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                thread_instructions_provider: Some(provider.clone()),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start root");
    let expected_provider = root
        .thread
        .session
        .inherited_instructions()
        .await
        .thread_provider;
    let control = &root
        .thread
        .session
        .services
        .local_agent_runtime
        .control(root.thread.session.session_id());
    let sender_id =
        spawn_v2_reload_test_child(control, harness.config.clone(), &root.thread, "sender")
            .await
            .thread_id;
    let target_id = match reload_target {
        InstructionsReloadTarget::Root => root.thread_id,
        InstructionsReloadTarget::Sibling => {
            spawn_v2_reload_test_child(control, harness.config.clone(), &root.thread, "target")
                .await
                .thread_id
        }
    };
    let sender = harness
        .manager
        .get_thread(sender_id)
        .await
        .expect("loaded sender");
    let target = harness
        .manager
        .get_thread(target_id)
        .await
        .expect("loaded target");
    persist_thread_for_tree_resume(&root.thread, "root persisted").await;
    persist_thread_for_tree_resume(&sender, "sender persisted").await;
    if matches!(reload_target, InstructionsReloadTarget::Sibling) {
        persist_thread_for_tree_resume(&target, "target persisted").await;
        target.shutdown_and_wait().await.expect("shut down target");
        assert!(harness.manager.remove_thread(&target_id).await.is_some());
    }
    root.thread
        .shutdown_and_wait()
        .await
        .expect("shut down root");
    assert!(
        harness
            .manager
            .remove_thread(&root.thread_id)
            .await
            .is_some()
    );
    assert_thread_not_loaded(&harness.manager, root.thread_id).await;
    drop(root);
    drop(target);

    *provider.text.write().expect("update thread instructions") =
        "updated while target was unloaded";
    let sender_turn = sender.session.new_default_turn().await;
    sender
        .session
        .services
        .agent_control
        .send(crate::SendRequest {
            caller: sender_id,
            target: crate::AgentTarget::Id(target_id),
            resume_config: crate::agent::child_config::build_agent_resume_config(&sender_turn)
                .expect("capture resume config"),
            input: crate::AgentInput::Message {
                message: AgentMessage::Plaintext("wake the unloaded target".to_string()),
                mode: MessageDeliveryMode::QueueOnly,
            },
            start_options: TurnStartOptions {
                root_turn_id: sender_turn.turn_metadata_state.root_turn_id(),
                turn_trigger: sender_turn.turn_metadata_state.current_turn_trigger(),
                cyber_access_program: sender_turn.cyber_access_program,
                ..Default::default()
            },
        })
        .await
        .expect("reload target from a surviving child");
    let resumed = harness
        .manager
        .get_thread(target_id)
        .await
        .expect("reloaded target");
    let inherited = resumed.session.inherited_instructions().await;
    assert_eq!(
        inherited
            .thread_provider
            .as_ref()
            .map(|provider| expected_provider
                .as_ref()
                .is_some_and(|root| Arc::ptr_eq(provider, root))),
        shared.then_some(true)
    );
    assert_eq!(
        inherited.thread.map(|instructions| instructions.text),
        shared.then(|| "updated while target was unloaded".to_string())
    );

    *provider.text.write().expect("update thread instructions") =
        "updated after target was reloaded";
    resumed
        .session
        .capture_step_context(
            resumed.session.new_default_turn().await,
            &CancellationToken::new(),
        )
        .await
        .expect("capture next request");
    assert_eq!(
        resumed
            .session
            .inherited_instructions()
            .await
            .thread
            .map(|instructions| instructions.text),
        shared.then(|| "updated after target was reloaded".to_string())
    );
}

#[tokio::test]
async fn resumed_root_reuses_or_freezes_surviving_shared_instructions() {
    let harness = AgentControlHarness::new().await;
    let id = ThreadId::new();
    let original = Arc::new(TestThreadInstructionsProvider {
        text: "shared snapshot".into(),
        shared: true,
    });
    let shared = harness
        .manager
        .agent_control()
        .runtime
        .root_thread_instructions_provider(id, Some(original.clone()))
        .expect("shared provider");
    let snapshot = shared.load_thread_instructions().await;
    let reused = harness
        .manager
        .agent_control()
        .runtime
        .root_thread_instructions_provider(id, /*provider*/ None)
        .expect("reuse the live tree's provider");
    assert!(Arc::ptr_eq(&shared, &reused));
    assert_eq!(reused.load_thread_instructions().await, snapshot);

    let reused = harness
        .manager
        .agent_control()
        .runtime
        .root_thread_instructions_provider(id, Some(reused))
        .expect("reuse the shared wrapper supplied during root reload");
    assert!(Arc::ptr_eq(&shared, &reused));
    *original.text.write().expect("update shared provider") = "updated shared instructions";
    assert_eq!(
        reused.load_thread_instructions().await,
        original.load_thread_instructions().await
    );

    let replacement = Arc::new(TestThreadInstructionsProvider {
        text: "replacement shared instructions".into(),
        shared: true,
    });
    let replaced = harness
        .manager
        .agent_control()
        .runtime
        .root_thread_instructions_provider(id, Some(replacement.clone()))
        .expect("replace the underlying provider");
    assert!(Arc::ptr_eq(&shared, &replaced));
    let snapshot = replacement.load_thread_instructions().await;
    assert_eq!(reused.load_thread_instructions().await, snapshot);

    let private = Arc::new(TestThreadInstructionsProvider {
        text: "private replacement".into(),
        shared: false,
    });
    let root_only = harness
        .manager
        .agent_control()
        .runtime
        .root_thread_instructions_provider(id, Some(private.clone()))
        .expect("private root provider");
    *original.text.write().expect("update old provider") = "stale shared update";
    *replacement.text.write().expect("update replaced provider") = "another stale shared update";
    assert_eq!(shared.load_thread_instructions().await, snapshot);
    assert_eq!(
        root_only.load_thread_instructions().await,
        private.load_thread_instructions().await
    );
}

#[tokio::test]
async fn spawn_agent_creates_thread_and_sends_prompt() {
    let harness = AgentControlHarness::new().await;
    let thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("spawned"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");
    let thread = harness
        .manager
        .get_thread(thread_id)
        .await
        .expect("thread should be registered");
    wait_for_recorded_user_message(thread.as_ref(), "spawned").await;
}

#[test_case::test_case(None, ReasoningEffort::Medium; "model default")]
#[test_case::test_case(Some(ReasoningEffort::Ultra), ReasoningEffort::XHigh; "resolved ultra")]
#[tokio::test]
async fn v2_spawn_resolves_reported_effort_without_changing_child_selection(
    selected_effort: Option<ReasoningEffort>,
    reported_effort: ReasoningEffort,
) {
    let (home, mut config) = test_config().await;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("enable v2");
    config.model_reasoning_effort = selected_effort.clone();
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    let source = thread_spawn_source(
        parent_id,
        &parent.session_source,
        next_thread_spawn_depth(&parent.session_source),
        /*agent_role*/ None,
        Some("worker".to_string()),
    )
    .expect("child source");
    let prepared = harness
        .control
        .prepare_spawn_fixture(&harness.config, &source, /*full_history*/ false)
        .await
        .expect("prepare child");
    let (agent, snapshot) = harness
        .control
        .spawn(SpawnRequest {
            caller: parent_id,
            config: prepared,
            input: AgentInput::Message {
                message: AgentMessage::Plaintext("child task".to_string()),
                mode: MessageDeliveryMode::TriggerTurn,
            },
            source,
            options: SpawnAgentOptions {
                parent_thread_id: Some(parent_id),
                ..Default::default()
            },
        })
        .await
        .expect("spawn child");
    let child = harness
        .manager
        .get_thread(agent.thread_id)
        .await
        .expect("child is registered");
    assert_eq!(
        (
            snapshot.reasoning_effort,
            child.config_snapshot().await.reasoning_effort,
        ),
        (Some(reported_effort), selected_effort),
    );
    child.shutdown_and_wait().await.expect("shutdown child");
    parent.shutdown_and_wait().await.expect("shutdown parent");
}

/// Pending configuration reaches descendants even when their root metadata differs.
#[tokio::test]
async fn pending_environment_failure_reaches_child_and_grandchild() {
    let (home, mut config) = test_config().await;
    for feature in [
        Feature::DeferredExecutor,
        Feature::MultiAgentV2,
        Feature::Sqlite,
    ] {
        config.features.enable(feature).expect("enable feature");
    }
    config.model = Some("gpt-5.6-sol".to_string());
    config.agent_max_depth = 2;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let cwd = PathUri::from_abs_path(&harness.config.codex_home);
    let pending = TurnEnvironmentSelection {
        selected_capability_roots: Default::default(),
        environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        cwd: cwd.clone(),
        workspace_roots: vec![cwd.clone()],
        config: EnvironmentConfigState::Pending,
    };
    // Only the second root's environment is selected. Inherited root lists can therefore
    // give this root a different position without changing which configuration to follow.
    let roots = ["unselected", codex_exec_server::LOCAL_ENVIRONMENT_ID]
        .into_iter()
        .map(|environment_id| SelectedCapabilityRoot {
            id: environment_id.to_string(),
            location: CapabilityRootLocation::Environment {
                environment_id: environment_id.to_string(),
                path: cwd.clone(),
            },
        })
        .collect::<Vec<_>>();
    let mut thread_extension_init = ExtensionDataInit::new();
    thread_extension_init.insert(roots);
    let root = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                environments: Some(vec![pending.clone().into_request()]),
                thread_extension_init,
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start root")
        .thread;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let mut parent = Arc::clone(&root);
    let mut descendants = Vec::new();
    for name in ["child", "grandchild"] {
        let agent =
            spawn_v2_reload_test_child(&control, harness.config.clone(), &parent, name).await;
        let thread = harness
            .manager
            .get_thread(agent.thread_id)
            .await
            .expect("get descendant");
        let selections = thread.environment_selections().await;
        assert_eq!(
            selections
                .clone()
                .into_iter()
                .map(TurnEnvironmentSelection::into_request)
                .collect::<Vec<_>>(),
            vec![pending.clone().into_request()],
        );
        descendants.push((Arc::clone(&thread), selections));
        parent = thread;
    }

    let error = "root could not prepare the environment";
    root.environment_failed(&pending, error.to_string())
        .await
        .expect("fail root environment");
    timeout(Duration::from_secs(/*secs*/ 5), async {
        for (thread, mut expected) in descendants {
            for selection in &mut expected {
                selection.config = EnvironmentConfigState::Failed(error.to_string());
            }
            while thread.environment_selections().await != expected {
                sleep(Duration::from_millis(/*millis*/ 10)).await;
            }
        }
    })
    .await
    .expect("both descendants should receive the root failure");
}

#[tokio::test]
async fn ephemeral_spawn_does_not_persist_agent_graph_edge() {
    let (home, mut config) = test_config().await;
    config.ephemeral = true;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;
    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("spawned"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
        )
        .await
        .expect("ephemeral agent spawn should succeed");

    let persisted_children = harness
        .state_db
        .as_ref()
        .expect("manager should retain state db")
        .list_thread_spawn_children(parent_thread_id)
        .await
        .expect("persisted child list should load");
    assert_eq!(persisted_children, Vec::<ThreadId>::new());
    assert!(
        harness.manager.get_thread(child_thread_id).await.is_ok(),
        "ephemeral child should remain live"
    );
}

/// New children inherit omitted plugin choices independently of copied history;
/// explicit choices, including an empty list, override the parent's selection.
#[test_case::test_case(None, None, vec!["parent-plugin".to_string()]; "fresh inherits")]
#[test_case::test_case(None, Some(vec!["child-plugin".to_string()]), vec!["child-plugin".to_string()]; "fresh overrides")]
#[test_case::test_case(None, Some(Vec::new()), Vec::new(); "fresh clears")]
#[test_case::test_case(Some(SpawnAgentForkMode::FullHistory), None, vec!["parent-plugin".to_string()]; "fork inherits")]
#[test_case::test_case(Some(SpawnAgentForkMode::FullHistory), Some(vec!["child-plugin".to_string()]), vec!["child-plugin".to_string()]; "fork overrides")]
#[test_case::test_case(Some(SpawnAgentForkMode::FullHistory), Some(Vec::new()), Vec::new(); "fork clears")]
#[tokio::test]
async fn spawn_agent_disabled_plugins_follow_parent_unless_overridden(
    fork_mode: Option<SpawnAgentForkMode>,
    disabled_plugin_ids: Option<Vec<String>>,
    expected: Vec<String>,
) {
    let harness = AgentControlHarness::new().await;
    let parent = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Paginated),
                environments: Some(Vec::new()),
                disabled_plugin_ids: Some(vec!["parent-plugin".to_string()]),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start parent with a disabled plugin");
    let parent_spawn_call_id = "spawn-plugin-child";
    let parent_turn = parent.thread.session.new_default_turn().await;
    parent
        .thread
        .session
        .record_conversation_items(
            parent_turn.as_ref(),
            parent_turn.model_info(),
            &[spawn_agent_call(parent_spawn_call_id)],
        )
        .await;
    let child_thread_id = harness
        .spawn_anonymous_child(
            parent.thread_id,
            SpawnAgentOptions {
                fork_parent_spawn_call_id: fork_mode
                    .as_ref()
                    .map(|_| parent_spawn_call_id.to_string()),
                fork_mode,
                disabled_plugin_ids,
                ..Default::default()
            },
        )
        .await;
    let child = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child should be registered");
    assert_eq!(
        child.thread_settings_snapshot().await.disabled_plugin_ids,
        expected
    );

    harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("shut down child");
    parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("shut down parent");
}

#[test_case::test_case(false; "parent in session source")]
#[test_case::test_case(true; "parent in session source and options")]
#[tokio::test]
async fn spawn_agent_fork_from_paginated_parent_uses_model_context_prefix(
    include_parent_in_options: bool,
) {
    let harness = AgentControlHarness::new().await;
    let client_mcp_extensions =
        ClientMcpExtensions::new([(OPENAI_FORM_EXTENSION_ID.to_string(), serde_json::json!({}))]);
    let parent = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Paginated),
                environments: Some(Vec::new()),
                client_mcp_extensions: client_mcp_extensions.clone(),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start paginated parent");
    let parent_thread_id = parent.thread_id;
    let parent_thread = parent.thread;
    parent_thread
        .inject_response_items(vec![user_message("paginated parent context")])
        .await
        .expect("inject paginated parent context");
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-paginated".to_string();
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            turn_context.model_info(),
            &[spawn_agent_call(&parent_spawn_call_id)],
        )
        .await;
    parent_thread
        .session
        .persist_rollout_items(&[
            rollout_response_item(ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "id-less inherited context".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
            RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: parent_thread_id,
                turn_id: "parent-turn".to_string(),
                item: TurnItem::UserMessage(UserMessageItem {
                    id: "parent-user".to_string(),
                    client_id: None,
                    content: Vec::new(),
                }),
                started_at_ms: Some(0),
                completed_at_ms: 1,
            })),
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                ThreadSettingsAppliedEvent {
                    thread_id: Some(parent_thread_id),
                    thread_settings: ThreadSettingsSnapshot {
                        disabled_plugin_ids: Vec::new(),
                        model: "parent-only-model".to_string(),
                        model_provider_id: "parent-only-provider".to_string(),
                        service_tier: None,
                        approval_policy: AskForApproval::Never,
                        approvals_reviewer: ApprovalsReviewer::User,
                        permission_profile: PermissionProfile::workspace_write(),
                        active_permission_profile: None,
                        cwd: harness.config.cwd.clone(),
                        runtime_workspace_roots: None,
                        reasoning_effort: None,
                        reasoning_summary: None,
                        personality: None,
                        collaboration_mode: CollaborationMode {
                            mode: ModeKind::Default,
                            settings: Settings {
                                model: "parent-only-model".to_string(),
                                reasoning_effort: None,
                                developer_instructions: None,
                            },
                        },
                    },
                },
            )),
        ])
        .await;

    let child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                // The source identifies the parent in both cases. Fork startup must
                // resolve it before inheriting client MCP settings, not just for metadata.
                parent_thread_id: include_parent_in_options.then_some(parent_thread_id),
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    assert!(
        history_contains_text(
            child_thread.session.clone_history().await.raw_items(),
            "paginated parent context",
        ),
        "bounded parent context should remain model-visible to the child"
    );
    child_thread.ensure_rollout_materialized().await;
    child_thread
        .flush_rollout()
        .await
        .expect("child rollout should flush");
    let rollout_path = child_thread
        .rollout_path()
        .expect("child rollout should exist");
    let lines = std::fs::read_to_string(&rollout_path)
        .expect("read child rollout")
        .lines()
        .map(|line| codex_rollout::parse_rollout_line(line).expect("parse rollout line"))
        .collect::<Vec<_>>();
    let RolloutItem::SessionMeta(meta_line) = &lines[0].item else {
        panic!("child rollout should start with session metadata");
    };
    assert_eq!(meta_line.meta.history_mode, ThreadHistoryMode::Paginated);
    assert_eq!(meta_line.meta.parent_thread_id, Some(parent_thread_id));
    assert_eq!(meta_line.meta.forked_from_id, Some(parent_thread_id));
    assert_eq!(child_thread.client_mcp_extensions(), client_mcp_extensions);
    let prefix_end = usize::try_from(
        meta_line
            .meta
            .subagent_history_start_ordinal
            .expect("paginated child should mark its local history boundary"),
    )
    .expect("history boundary should fit in usize");
    let copied_prefix = &lines[1..prefix_end];
    let copied_idless_context = copied_prefix
        .iter()
        .find_map(|line| match &line.item {
            RolloutItem::ResponseItem(response_item)
                if serde_json::to_string(&response_item.item)
                    .expect("serialize response item")
                    .contains("id-less inherited context") =>
            {
                Some(response_item)
            }
            _ => None,
        })
        .expect("copied prefix should contain inherited response item");
    assert!(
        copied_idless_context.id().is_some_and(|id| !id.is_empty()),
        "copied model context should receive response item ids before persistence"
    );
    let copied_parent_context_count = lines
        .iter()
        .filter(|line| {
            serde_json::to_string(&line.item)
                .expect("serialize rollout item")
                .contains("paginated parent context")
        })
        .count();
    assert_eq!(
        copied_parent_context_count, 1,
        "copied model context should be persisted once"
    );
    assert!(
        !copied_prefix.iter().any(|line| {
            matches!(
                &line.item,
                RolloutItem::EventMsg(
                    EventMsg::ItemCompleted(_) | EventMsg::ThreadSettingsApplied(_)
                )
            )
        }),
        "copied non-structural presentation and metadata records should not enter the child rollout"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[test_case::test_case(MultiAgentVersion::V1, true, false; "v1_gate_enabled")]
#[test_case::test_case(MultiAgentVersion::V2, false, false; "v2_gate_disabled")]
#[test_case::test_case(MultiAgentVersion::V2, true, true; "v2_gate_enabled")]
#[tokio::test]
async fn spawn_agent_without_fork_from_paginated_parent_stays_fresh_and_paginated(
    multi_agent_version: MultiAgentVersion,
    dynamic_tools_enabled: bool,
    inherits_dynamic_tools: bool,
) {
    let mut harness = AgentControlHarness::new().await;
    let features = &mut harness.config.features;
    features
        .disable(Feature::MultiAgentV2)
        .expect("disable raw v2 feature");
    if dynamic_tools_enabled {
        features.enable(Feature::MultiAgentV2DynamicTools)
    } else {
        features.disable(Feature::MultiAgentV2DynamicTools)
    }
    .expect("configure dynamic tool inheritance");
    let dynamic_tools = vec![codex_protocol::dynamic_tools::DynamicToolSpec::Function(
        codex_protocol::dynamic_tools::DynamicToolFunctionSpec {
            name: "echo".to_string(),
            description: "Return the supplied message.".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {"message": {"type": "string"}}}),
            defer_loading: false,
        },
    )];
    let parent = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Paginated),
                environments: Some(Vec::new()),
                dynamic_tools: dynamic_tools.clone(),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start parent with dynamic tools");
    let parent_thread_id = parent.thread_id;
    let parent_thread = parent.thread;
    assert_eq!(
        parent_thread
            .session
            .set_multi_agent_version_if_unset(multi_agent_version),
        multi_agent_version
    );
    parent_thread
        .inject_response_items(vec![user_message("parent-only context")])
        .await
        .expect("inject parent-only context");

    let child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let expected_dynamic_tools = if inherits_dynamic_tools {
        dynamic_tools
    } else {
        Vec::new()
    };
    assert_eq!(
        child_thread.session.dynamic_tools().await,
        expected_dynamic_tools
    );
    assert!(
        !history_contains_text(
            child_thread.session.clone_history().await.raw_items(),
            "parent-only context",
        ),
        "fork_turns=none should not copy parent context"
    );
    child_thread.ensure_rollout_materialized().await;
    child_thread
        .flush_rollout()
        .await
        .expect("child rollout should flush");
    let meta = codex_rollout::read_session_meta_line(
        &child_thread
            .rollout_path()
            .expect("child rollout should exist"),
    )
    .await
    .expect("read child session metadata");
    assert_eq!(meta.meta.history_mode, ThreadHistoryMode::Paginated);
    assert_eq!(meta.meta.subagent_history_start_ordinal, None);
    assert_eq!(
        meta.meta.dynamic_tools.unwrap_or_default(),
        expected_dynamic_tools
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_fork_sanitizes_inherited_compaction_metadata() {
    let mut harness = AgentControlHarness::new().await;
    let _ = harness.config.features.disable(Feature::MultiAgentV2);

    let (parent_thread_id, parent_thread) = harness.start_paginated_thread().await;
    let parent_resume_metadata = codex_history::CompactionResumeMetadata {
        multi_agent_version: Some(MultiAgentVersion::V2),
        last_started_turn_id: Some("parent-turn".to_string()),
        turn_attribution: Some(codex_history::TurnAttribution {
            turn_id: "parent-turn".to_string(),
            turn_trigger: Some("automation".to_string()),
            parent_turn_id: Some("initiating-turn".to_string()),
            initiating_agent_path: Some(codex_protocol::AgentPath::root()),
            root_turn_id: Some("root-turn".to_string()),
        }),
        previous_turn_settings: Some(codex_history::PreviousTurnSettings {
            model: "parent-model".into(),
            comp_hash: None,
            cyber_access_program: None,
            realtime_active: None,
        }),
    };
    let parent_usage = TokenUsage {
        total_tokens: 120,
        ..TokenUsage::default()
    };
    let parent_record = TokenUsageRecord {
        thread_id: parent_thread_id,
        turn_id: "parent-turn".to_string(),
        session_id: parent_thread.session.session_id(),
        root_turn_id: "parent-turn".to_string(),
        response_id: "parent-response".to_string(),
        usage: parent_usage.clone(),
        turn_token_usage: parent_usage.clone(),
        thread_token_usage: parent_usage,
    };
    let parent_spawn_call_id = "spawn-call-token-usage".to_string();
    parent_thread
        .session
        .persist_rollout_items(&[
            RolloutItem::Compacted(Box::new(CompactedItem {
                message: String::new(),
                replacement_history: Some(vec![user_message("compacted parent context").into()]),
                retained_context: None,
                guardian_history: None,
                mcp_resource_origins: None,
                window_number: Some(1),
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                compaction_response_id: None,
                latest_token_usage_record: Some(parent_record.clone()),
                resume_metadata: Some(parent_resume_metadata.clone()),
            })),
            RolloutItem::TokenUsageRecord(parent_record),
            rollout_response_item(spawn_agent_call(&parent_spawn_call_id)),
        ])
        .await;

    let child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");

    let child_usage = TokenUsage {
        total_tokens: 80,
        ..TokenUsage::default()
    };
    assert!(
        !child_thread
            .session
            .clone_history()
            .await
            .retained_context()
            .user_messages_complete(),
        "V1 forks lack complete retained authorization in both context modes"
    );
    let turn_context = child_thread.session.new_default_turn().await;
    child_thread
        .session
        .record_observed_response_completed(
            turn_context.as_ref(),
            "child-response",
            Some(&child_usage),
            /*usage_metadata*/ None,
        )
        .await;
    child_thread
        .flush_rollout()
        .await
        .expect("child rollout should flush");
    let rollout_path = child_thread
        .rollout_path()
        .expect("child rollout should exist");
    let lines = std::fs::read_to_string(&rollout_path)
        .expect("read child rollout")
        .lines()
        .map(|line| codex_rollout::parse_rollout_line(line).expect("parse rollout line"))
        .collect::<Vec<_>>();
    assert!(
        !lines.iter().any(|line| {
            matches!(
                &line.item,
                RolloutItem::TokenUsageRecord(record) if record.thread_id == parent_thread_id
            )
        }),
        "child rollout should not inherit parent token usage records"
    );
    assert!(
        lines.iter().all(|line| {
            !matches!(
                &line.item,
                RolloutItem::Compacted(compacted)
                    if compacted.latest_token_usage_record.is_some()
            )
        }),
        "child rollout should not inherit parent token usage checkpoints"
    );
    let inherited_resume_metadata = lines
        .iter()
        .find_map(|line| match &line.item {
            RolloutItem::Compacted(item) => item.resume_metadata.as_ref(),
            _ => None,
        })
        .expect("inherited checkpoint");
    assert_eq!(
        inherited_resume_metadata,
        &codex_history::CompactionResumeMetadata {
            multi_agent_version: Some(MultiAgentVersion::V1),
            turn_attribution: None,
            ..parent_resume_metadata
        }
    );
    let child_record = lines.iter().rev().find_map(|line| match &line.item {
        RolloutItem::TokenUsageRecord(record) => Some(record),
        _ => None,
    });
    assert_eq!(
        child_record,
        Some(&TokenUsageRecord {
            thread_id: child_thread_id,
            turn_id: turn_context.sub_id.clone(),
            session_id: child_thread.session.session_id(),
            root_turn_id: turn_context.sub_id.clone(),
            response_id: "child-response".to_string(),
            usage: child_usage.clone(),
            turn_token_usage: child_usage.clone(),
            thread_token_usage: child_usage,
        })
    );
}

#[tokio::test]
async fn spawn_agent_can_fork_parent_thread_history_with_sanitized_items() {
    let managed_fragment = "<managed_developer_instructions>\nParent developer instructions.\n</managed_developer_instructions>";
    let persistent_fragment =
        "<persistent_mode>\nParent developer instructions.\n</persistent_mode>";
    let replacement_fragment = ModelInstructions {
        model: None,
        instructions: Some("Updated model instructions.".to_string()),
    }
    .render()
    .expect_text();
    let harness = AgentControlHarness::new().await;
    let mut parent_config = harness.config.clone();
    let _ = parent_config.features.enable(Feature::MultiAgentV2);
    parent_config.developer_instructions = Some("Parent developer instructions.".to_string());
    parent_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Parent root guidance.".to_string());
    parent_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Parent subagent guidance.".to_string());
    let mut child_config = harness.config.clone();
    child_config.multi_agent_v2.preserve_fork_prefix = false;
    let _ = child_config.features.enable(Feature::MultiAgentV2);
    child_config.developer_instructions = Some("Child developer instructions.".to_string());
    child_config.multi_agent_v2.subagent_developer_instructions =
        Some("Child developer instructions.".to_string());
    parent_config.multi_agent_v2.subagent_developer_instructions = child_config
        .multi_agent_v2
        .subagent_developer_instructions
        .clone();
    parent_config.multi_agent_v2.preserve_fork_prefix =
        child_config.multi_agent_v2.preserve_fork_prefix;
    let new_thread = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..StartThreadOptions::new(parent_config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start parent thread");
    let parent_thread_id = new_thread.thread_id;
    let parent_thread = new_thread.thread;
    parent_thread
        .session
        .inject_no_new_turn(
            vec![user_message("parent seed context")],
            /*current_turn_context*/ None,
        )
        .await;
    let expected_parent_seed = parent_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .next()
        .cloned()
        .expect("parent seed should be recorded");
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-history".to_string();
    let trigger_message = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "parent trigger message".to_string(),
        /*trigger_turn*/ true,
    );
    let tool_declarations = ResponseItem::AdditionalTools {
        id: None,
        role: "developer".to_string(),
        tools: Arc::from([serde_json::json!({"type": "function", "name": "parent_tool"})]),
    };
    let tool_state = serde_json::json!({"top_level_tools": {"function:parent_tool": "hash"}})
        .as_object()
        .unwrap()
        .clone();
    let standalone_output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: None,
        name: Some("notifications".to_string()),
        namespace: Some("slack".to_string()),
        output: FunctionCallOutputPayload::from_text("parent notification".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(), turn_context.model_info(),
            &[
                ResponseItem::Message {
                    status: None,
                    encrypted_content: None,
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "Parent root guidance.".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    status: None,
                    encrypted_content: None,
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "Parent subagent guidance.".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    status: None,
                    encrypted_content: None,
                    id: None,
                    role: "developer".to_string(),
                    content: vec![
                        ContentItem::InputText {
                            text: "Developer context before.\nParent developer instructions.\nDeveloper context after."
                                .to_string(),
                        },
                        ContentItem::InputText {
                            text: "<multi_agent_mode>Proactive multi-agent delegation is active.</multi_agent_mode>"
                                .to_string(),
                        },
                        ContentItem::InputText {
                            text: "Preserved developer context.".to_string(),
                        },
                        ContentItem::InputText {
                            text: managed_fragment.to_string(),
                        },
                        ContentItem::InputText {
                            text: persistent_fragment.to_string(),
                        },
                        ContentItem::InputText {
                            text: replacement_fragment.clone(),
                        },
                    ],
                    phase: None,
                    internal_chat_message_metadata_passthrough: Some(
                        InternalChatMessageMetadataPassthrough {
                            content_item_kinds: Some(vec![
                                ContentItemKind("generic.developer_instructions".to_string()),
                                ContentItemKind("multi_agent.mode_instructions".to_string()),
                                ContentItemKind("generic.developer_policy".to_string()),
                                ContentItemKind("managed_config.developer_instructions".to_string()),
                                ContentItemKind("persistent_mode.instructions".to_string()),
                                ContentItemKind("model.instructions".to_string()),
                            ]),
                            ..Default::default()
                        },
                    ),
                },
                assistant_message("parent commentary", Some(MessagePhase::Commentary)),
                assistant_message("parent answer fragment", Some(MessagePhase::PartialAnswer)),
                assistant_message("parent final answer", Some(MessagePhase::FinalAnswer)),
                standalone_output,
                assistant_message("parent unknown phase", /*phase*/ None),
                ResponseItem::Reasoning {
                    id: Some(ResponseItemId::with_suffix("rs", "parent-reasoning")),
                    summary: Vec::new(),
                    content: None,
                    encrypted_content: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                trigger_message.to_response_input_item().into(),
                tool_declarations.clone(),
                spawn_agent_call(&parent_spawn_call_id),
            ],
        )
        .await;
    let expected_standalone_output = parent_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .find(|item| matches!(item, ResponseItem::FunctionCallOutput { call_id: None, .. }))
        .cloned()
        .expect("standalone output should be recorded");
    let parent_reference_context_item = turn_context.to_turn_context_item();
    parent_thread
        .session
        .persist_rollout_items(&[
            RolloutItem::WorldState(WorldStateItem::full(tool_state.clone())),
            RolloutItem::TurnContext(parent_reference_context_item.clone()),
        ])
        .await;
    parent_thread
        .session
        .ensure_rollout_materialized(PersistContext::Standard)
        .await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");
    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                multi_agent_v2_usage_hints: Some(ResolvedMultiAgentV2UsageHints {
                    root: None,
                    subagent: Some(MultiAgentRoleInstructions::Configured(
                        "Child subagent guidance.".into(),
                    )),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should succeed")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    assert_ne!(child_thread_id, parent_thread_id);
    assert_eq!(
        child_thread.config_snapshot().await.history_mode,
        ThreadHistoryMode::Legacy
    );
    let history = child_thread.session.clone_history().await;
    let history_items = history.raw_items().cloned().collect::<Vec<_>>();
    let assistant_position = |history: &crate::context_manager::ContextManager| {
        history.annotated_items().iter().find(|item| {
            matches!(&item.item, ResponseItem::Message { role, phase: Some(MessagePhase::FinalAnswer), .. } if role == "assistant")
        }).expect("recorded final answer").metadata.as_ref().and_then(|metadata| metadata.user_input_order)
    };
    assert!(assistant_position(&parent_thread.session.clone_history().await).is_some());
    assert_eq!(assistant_position(&history), None);
    let expected_partial_answer = parent_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .find(|item| {
            matches!(
                item,
                ResponseItem::Message {
                    phase: Some(MessagePhase::PartialAnswer),
                    ..
                }
            )
        })
        .cloned()
        .expect("parent answer fragment should be recorded");
    let expected_final_answer = parent_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .find(|item| {
            matches!(
                item,
                ResponseItem::Message {
                    role,
                    phase: Some(MessagePhase::FinalAnswer),
                    ..
                } if role == "assistant"
            )
        })
        .cloned()
        .expect("parent final answer should be recorded");
    let mut expected_developer_message = ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "developer".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "Developer context before.\nChild developer instructions.\nDeveloper context after."
                    .to_string(),
            },
            ContentItem::InputText {
                text: "Preserved developer context.".to_string(),
            },
            ContentItem::InputText {
                text: managed_fragment.to_string(),
            },
            ContentItem::InputText {
                text: persistent_fragment.to_string(),
            },
            ContentItem::InputText {
                text: replacement_fragment,
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(
            InternalChatMessageMetadataPassthrough {
                content_item_kinds: Some(vec![
                    ContentItemKind("generic.developer_instructions".to_string()),
                    ContentItemKind("generic.developer_policy".to_string()),
                    ContentItemKind("managed_config.developer_instructions".to_string()),
                    ContentItemKind("persistent_mode.instructions".to_string()),
                    ContentItemKind("model.instructions".to_string()),
                ]),
                ..Default::default()
            },
        ),
    };
    expected_developer_message.set_turn_id_if_missing(&turn_context.sub_id);
    expected_developer_message.set_create_time_if_missing(
        history_items[1]
            .executed_tool_call_metadata()
            .and_then(|metadata| metadata.create_time.clone())
            .expect("recorded developer message should have a creation timestamp"),
    );
    let expected_history = [
        expected_parent_seed,
        expected_developer_message,
        expected_partial_answer,
        expected_final_answer,
        expected_standalone_output,
        tool_declarations,
        ContextualUserFragment::into(MultiAgentRoleInstructions::Configured(
            "Child subagent guidance.".to_string(),
        )),
    ];
    assert_eq!(
        strip_response_item_ids(&history_items),
        strip_response_item_ids(&expected_history),
        "full-history forked child history should replace parent usage hints with the child subagent hint while filtering non-final assistant/tool chatter"
    );
    assert_eq!(
        serde_json::to_value(child_thread.session.reference_context_item().await)
            .expect("serialize child reference context item"),
        serde_json::to_value(Some(parent_reference_context_item))
            .expect("serialize expected reference context item"),
        "full-history forked child should preserve the parent diff baseline"
    );

    assert_eq!(
        history.world_state_checkpoint().unwrap().state,
        tool_state,
        "full-history forks must retain the catalog baseline with its declarations"
    );

    let mut no_hint_parent_config = parent_config.clone();
    no_hint_parent_config.multi_agent_v2.preserve_fork_prefix = false;
    no_hint_parent_config
        .multi_agent_v2
        .subagent_developer_instructions = Some(String::new());
    let stored = parent_thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .expect("read parent fixture history");
    let no_hint_parent = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                initial_history: InitialHistory::Forked(
                    stored.history.expect("parent history").items,
                ),
                ..StartThreadOptions::new(no_hint_parent_config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start host-configured no-hint parent fixture");
    let mut no_hint_child_config = no_hint_parent_config;
    no_hint_child_config.developer_instructions = Some(String::new());
    let no_hint_child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            no_hint_child_config,
            text_input("child task without hints"),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: no_hint_parent.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                multi_agent_v2_usage_hints: Some(ResolvedMultiAgentV2UsageHints {
                    root: None,
                    subagent: None,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should honor an empty subagent usage hint")
        .thread_id;
    let no_hint_child_thread = harness
        .manager
        .get_thread(no_hint_child_thread_id)
        .await
        .expect("no-hint child thread should be registered");
    let no_hint_history = no_hint_child_thread.session.clone_history().await;
    assert!(
        !history_contains_text(no_hint_history.raw_items(), "Child subagent guidance.")
            && !history_contains_text(
                no_hint_history.raw_items(),
                "You are an agent in a team of agents"
            ),
        "full-history forked child should not add configured or bundled subagent guidance"
    );
    assert!(
        !history_contains_text(
            no_hint_history.raw_items(),
            "Developer context before.\nParent developer instructions."
        ),
        "empty child developer instructions should remove parent developer instructions"
    );
    assert!(
        history_contains_text(no_hint_history.raw_items(), managed_fragment)
            && history_contains_text(no_hint_history.raw_items(), persistent_fragment),
        "clearing child instructions must preserve overlapping managed and persistent instructions"
    );
    assert!(
        history_contains_text(
            no_hint_history.raw_items(),
            "Developer context before.\n\nDeveloper context after."
        ),
        "empty child developer instructions should preserve surrounding developer context"
    );
    assert!(
        history_contains_text(no_hint_history.raw_items(), "Preserved developer context."),
        "empty child developer instructions should preserve unrelated developer fragments"
    );

    wait_for_recorded_user_message(child_thread.as_ref(), "child task").await;

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = harness
        .control
        .shutdown_live_agent(no_hint_child_thread_id)
        .await
        .expect("no-hint child shutdown should submit");
    no_hint_parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("no-hint parent shutdown");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[test_case::test_case(true; "marked catalog hints")]
#[test_case::test_case(false; "unmarked bundled hints")]
#[tokio::test]
async fn spawn_agent_fork_strips_parent_usage_hints_from_compacted_history(marked: bool) {
    let harness = AgentControlHarness::new().await;
    let mut parent_config = harness.config.clone();
    let _ = parent_config.features.enable(Feature::MultiAgentV2);
    parent_config.developer_instructions = Some("Parent developer instructions.".to_string());
    parent_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Parent root guidance.".to_string());
    parent_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Parent subagent guidance.".to_string());
    let mut child_config = harness.config.clone();
    child_config.multi_agent_v2.preserve_fork_prefix = false;
    let _ = child_config.features.enable(Feature::MultiAgentV2);
    child_config.developer_instructions = Some("Child developer instructions.".to_string());
    child_config.multi_agent_v2.subagent_developer_instructions =
        Some("Child developer instructions.".to_string());
    parent_config.multi_agent_v2.subagent_developer_instructions = child_config
        .multi_agent_v2
        .subagent_developer_instructions
        .clone();
    parent_config.multi_agent_v2.preserve_fork_prefix =
        child_config.multi_agent_v2.preserve_fork_prefix;
    let new_thread = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions::new(parent_config),
            harness.control.clone(),
        )
        .await
        .expect("start parent thread");
    let parent_thread_id = new_thread.thread_id;
    let parent_thread = new_thread.thread;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-compacted-usage-hints".to_string();
    let catalog_role = |base: &str| MultiAgentRoleInstructions::Composed {
        base: base.to_string(),
        marked,
        omit_update_plan_instructions: false,
        max_concurrency: 2,
        wait_agent_enabled: false,
        expose_model_overrides: false,
    };
    let parent_task = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").expect("valid worker path"),
        Vec::new(),
        "compacted parent delegated task".to_string(),
        /*trigger_turn*/ true,
    );
    let tool_declarations = ResponseItem::AdditionalTools {
        id: None,
        role: "developer".to_string(),
        tools: Arc::from([serde_json::json!({"type": "function", "name": "compacted_tool"})]),
    };
    let tools_update = ContextualUserFragment::into(IncrementalToolsHint(
        codex_prompts::ResolvedIncrementalToolMessages::default().tool_update_hint,
    ));
    let replacement_history = vec![
        tools_update.clone(),
        tool_declarations.clone(),
        ContextualUserFragment::into(crate::context::GuardianApprovedAction::new("parent-private-release".to_owned())),
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "compacted parent summary".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ContextualUserFragment::into(catalog_role("Catalog parent root guidance.")),
        parent_task.to_model_input_item(),
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "Previous parent root guidance.".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![ContentItemKind("multi_agent.usage_hint".to_string())]),
                    ..Default::default()
                },
            ),
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "developer".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "Compacted context before.\nParent developer instructions.\nCompacted context after."
                        .to_string(),
                },
                ContentItem::InputText {
                    text: "<multi_agent_mode>Proactive multi-agent delegation is active.</multi_agent_mode>"
                        .to_string(),
                },
                ContentItem::InputText {
                    text: "Preserved compacted developer context.".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let answer_event: codex_history::RetainedContextEvent = serde_json::from_value(serde_json::json!({
        "type": "verified_answer", "turn_id": "parent-answer-turn", "call_id": "parent-answer-call",
        "questions": [{"question": "Parent-local action?", "answer": "Parent only."}]
    })).expect("verified answer fixture");
    let delivery = codex_history::ResponseItemEnvelope {
        item: serde_json::from_value(serde_json::json!({
            "type": "function_call_output", "id": "parent-delivery",
            "name": "send_message_to_thread", "output": "Parent delivery"
        }))
        .unwrap(),
        metadata: Some(codex_history::CodexHarnessMetadata {
            user_input_order: Some(7),
            sender_user_messages: Some(Box::new(codex_history::SenderUserMessages {
                receiver_turn_id: "parent-turn".to_owned(),
                receiver_message_id: "parent-delivery".to_owned(),
                text: "Parent-only sender context".to_owned(),
            })),
            ..Default::default()
        }),
    };
    let mut retained_context = codex_history::RetainedContext::default();
    retained_context.record(&answer_event);
    retained_context.record_sender_user_messages(delivery.metadata.as_ref().unwrap());
    parent_thread
        .session
        .persist_rollout_items(&[
            RolloutItem::Compacted(Box::new(CompactedItem {
                message: String::new(),
                replacement_history: Some(
                    replacement_history
                        .into_iter()
                        .map(Into::into)
                        .chain([delivery.clone()])
                        .collect(),
                ),
                retained_context: Some(retained_context),
                guardian_history: Some(codex_history::GuardianHistoryCheckpoint(vec![
                    user_message("Parent-local approval must not be inherited.").into(),
                ])),
                mcp_resource_origins: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                compaction_response_id: None,
                latest_token_usage_record: None,
                resume_metadata: None,
            })),
            RolloutItem::RetainedContext(answer_event),
            RolloutItem::ResponseItem(delivery),
            RolloutItem::TurnContext(turn_context.to_turn_context_item()),
            rollout_response_item(spawn_agent_call(&parent_spawn_call_id)),
        ])
        .await;
    parent_thread
        .session
        .ensure_rollout_materialized(PersistContext::Standard)
        .await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                multi_agent_v2_usage_hints: Some(ResolvedMultiAgentV2UsageHints {
                    root: None,
                    subagent: Some(catalog_role("Catalog child subagent guidance.")),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should sanitize compacted usage hints")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert_eq!(
        strip_response_item_ids(
            &history
                .raw_items()
                .filter(|item| {
                    matches!(item, ResponseItem::AdditionalTools { .. })
                        || matches!(item, ResponseItem::Message {
                            internal_chat_message_metadata_passthrough: Some(metadata), ..
                        } if metadata.content_item_kinds.as_ref().is_some_and(|kinds| {
                            kinds.iter().any(|kind| kind.as_str() == IncrementalToolsHint::KIND)
                        }))
                })
                .cloned()
                .collect::<Vec<_>>()
        ),
        vec![tools_update, tool_declarations],
        "full-history forks must retain tool update notices with declarations embedded in compaction checkpoints"
    );
    assert!(
        !history_contains_text(
            history.conversation_history_snapshot().review_items(),
            "Parent-local approval must not be inherited.",
        ),
        "a subagent must not inherit its parent review checkpoint",
    );
    assert!(
        !history_contains_text(history.raw_items(), "parent-private-release"),
        "a subagent must not inherit parent-local approval messages",
    );
    let mut inherited_context = codex_history::RetainedContext::default();
    inherited_context.reserve_order();
    assert_eq!(history.retained_context(), &inherited_context);
    assert!(
        history_contains_text(history.raw_items(), "compacted parent summary"),
        "forked child history should retain compacted non-hint content"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Catalog parent root guidance."),
        "forked child history should strip the resolved parent hint from compacted replacement history"
    );
    assert!(
        history_contains_text(history.raw_items(), "Catalog child subagent guidance."),
        "full-history forked child should add the resolved child hint after compacted-history sanitization"
    );
    assert!(
        !history
            .raw_items()
            .any(|item| matches!(item, ResponseItem::AgentMessage { .. })),
        "forked child history should not inherit compacted parent agent messages"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Previous parent root guidance."),
        "forked child history should strip stale parent hints from compacted replacement history"
    );
    assert!(
        !history_contains_text(
            history.raw_items(),
            "Proactive multi-agent delegation is active."
        ),
        "forked child history should strip stale policy fragments from compound compacted messages"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Parent developer instructions."),
        "forked child history should replace parent instructions in compacted replacement history"
    );
    assert!(
        history_contains_text(
            history.raw_items(),
            "Compacted context before.\nChild developer instructions.\nCompacted context after."
        ),
        "forked child history should replace compacted parent instructions without removing surrounding context"
    );
    assert!(
        history_contains_text(
            history.raw_items(),
            "Preserved compacted developer context."
        ),
        "forked child history should preserve unrelated compacted developer fragments"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

/// Full-history forks must restore child instructions when compaction discarded
/// the only matching parent instruction fragment from effective history.
#[tokio::test]
async fn spawn_agent_full_fork_restores_instructions_after_compaction_discards_parent_fragment() {
    let harness = AgentControlHarness::new().await;
    let mut parent_config = harness.config.clone();
    let _ = parent_config.features.enable(Feature::MultiAgentV2);
    parent_config.developer_instructions = Some("Parent developer instructions.".to_string());
    let mut child_config = parent_config.clone();
    child_config.developer_instructions = Some("Child developer instructions.".to_string());
    child_config.multi_agent_v2.subagent_developer_instructions =
        Some("Child developer instructions.".to_string());

    parent_config.multi_agent_v2.subagent_developer_instructions = child_config
        .multi_agent_v2
        .subagent_developer_instructions
        .clone();
    parent_config.multi_agent_v2.preserve_fork_prefix =
        child_config.multi_agent_v2.preserve_fork_prefix;
    let new_thread = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions::new(parent_config),
            harness.control.clone(),
        )
        .await
        .expect("start parent thread");
    let parent_thread_id = new_thread.thread_id;
    let parent_thread = new_thread.thread;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-compacted-stale-instructions".to_string();
    let replacement_history = vec![
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "compacted parent summary".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "Preserved compacted developer context.".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    // Preserve the parent's live baseline while its durable checkpoint omits the
    // developer fragment that appeared in obsolete pre-compaction history.
    parent_thread
        .session
        .replace_history(
            replacement_history.clone(),
            Some(turn_context.to_turn_context_item()),
        )
        .await;
    parent_thread
        .session
        .persist_rollout_items(&[
            rollout_response_item(ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "Parent developer instructions.".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
            RolloutItem::Compacted(Box::new(CompactedItem {
                message: String::new(),
                replacement_history: Some(
                    replacement_history.into_iter().map(Into::into).collect(),
                ),
                retained_context: None,
                guardian_history: None,
                mcp_resource_origins: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                compaction_response_id: None,
                latest_token_usage_record: None,
                resume_metadata: None,
            })),
            RolloutItem::TurnContext(turn_context.to_turn_context_item()),
            rollout_response_item(spawn_agent_call(&parent_spawn_call_id)),
        ])
        .await;
    parent_thread
        .session
        .ensure_rollout_materialized(PersistContext::Standard)
        .await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should preserve effective compacted instructions")
        .thread_id;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(
            history.raw_items(),
            "Preserved compacted developer context."
        ),
        "full-history fork should preserve unrelated compacted developer fragments"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Parent developer instructions."),
        "full-history fork should not restore stale pre-compaction parent instructions"
    );
    assert!(
        history_contains_text(history.raw_items(), "Child developer instructions."),
        "full-history fork should append child instructions absent from effective compacted history"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

/// A legacy compaction clears the child's baseline, so its first turn must
/// rebuild configured developer instructions exactly once.
#[test_case::test_case(false; "filtered fork")]
#[test_case::test_case(true; "preserve prefix fallback")]
#[tokio::test]
async fn spawn_agent_full_fork_legacy_compaction_rebuilds_child_instructions_once(
    preserve_fork_prefix: bool,
) {
    let managed_policy = "Managed policy for every agent.";
    let current_managed_fragment = format!(
        "<managed_developer_instructions>\n{managed_policy}\n</managed_developer_instructions>"
    );
    let stale_managed_fragment =
        "<managed_developer_instructions>\nOld managed policy.\n</managed_developer_instructions>";
    for (case, parent_developer_instructions) in [
        ("without parent instructions", None),
        (
            "with parent instructions",
            Some("Parent developer instructions."),
        ),
    ] {
        let mut harness = AgentControlHarness::new().await;
        let mut parent_config = harness.config.clone();
        let _ = parent_config.features.enable(Feature::MultiAgentV2);
        let _ = parent_config.features.enable(Feature::IncrementalTools);
        let mut model = model_info_from_slug("gpt-5.5");
        model.use_responses_lite = true;
        parent_config.model_catalog = Some(ModelsResponse {
            models: vec![model],
        });
        parent_config.base_instructions = Some("Current model instructions.".to_string());
        parent_config.developer_instructions = parent_developer_instructions.map(str::to_string);
        let mut requirements = parent_config.config_layer_stack.requirements().clone();
        requirements.additional_developer_instructions = Some(codex_config::Sourced::new(
            managed_policy.to_string(),
            codex_config::RequirementSource::Unknown,
        ));
        let mut requirements_toml = parent_config.config_layer_stack.requirements_toml().clone();
        requirements_toml.additional_developer_instructions = Some(managed_policy.to_string());
        parent_config.config_layer_stack = codex_config::ConfigLayerStack::new(
            parent_config
                .config_layer_stack
                .all_layers_low_to_high()
                .cloned()
                .collect(),
            requirements,
            requirements_toml,
        )
        .expect("managed requirements stack");
        let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
        harness.manager = ThreadManager::new(
            &parent_config,
            auth_manager.clone(),
            crate::thread_manager::build_models_manager(&parent_config, auth_manager),
            crate::CodexAppsToolsCache::default(),
            SessionSource::Exec,
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            empty_extension_registry(),
            Arc::new(crate::test_support::EmptyUserInstructionsProvider),
            /*analytics_events_client*/ None,
            crate::thread_manager::passthrough_image_store(),
            crate::thread_manager::thread_store_from_config(
                &parent_config,
                harness.state_db.clone(),
            ),
            crate::thread_manager::local_agent_graph_store_from_state_db(harness.state_db.as_ref()),
            uuid::Uuid::new_v4().to_string(),
            /*attestation_provider*/ None,
            /*external_time_provider*/ None,
        );
        harness.control = harness.manager.agent_control();
        let mut child_config = parent_config.clone();
        child_config.multi_agent_v2.preserve_fork_prefix = preserve_fork_prefix;
        child_config.developer_instructions = Some("Child developer instructions.".to_string());
        child_config.multi_agent_v2.subagent_developer_instructions =
            Some("Child developer instructions.".to_string());

        parent_config.multi_agent_v2.subagent_developer_instructions = child_config
            .multi_agent_v2
            .subagent_developer_instructions
            .clone();
        parent_config.multi_agent_v2.preserve_fork_prefix =
            child_config.multi_agent_v2.preserve_fork_prefix;
        let new_thread = harness
            .manager
            .start_thread_with_control_for_tests(
                StartThreadOptions::new(parent_config),
                harness.control.clone(),
            )
            .await
            .expect("start parent thread");
        let parent_thread_id = new_thread.thread_id;
        let parent_thread = new_thread.thread;
        let turn_context = parent_thread.session.new_default_turn().await;
        let parent_spawn_call_id = match parent_developer_instructions {
            Some(_) => "spawn-call-legacy-compact-with-parent",
            None => "spawn-call-legacy-compact-without-parent",
        };
        let parent_user_message = ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "parent task before legacy compaction".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };

        // A live parent can reestablish its baseline after resuming a rollout
        // whose older compaction record cannot restore that baseline to a child.
        parent_thread
            .session
            .replace_history(
                vec![parent_user_message.clone()],
                Some(turn_context.to_turn_context_item()),
            )
            .await;
        let mut rollout_items = vec![
            rollout_response_item(parent_user_message),
            RolloutItem::Compacted(Box::new(CompactedItem {
                message: "legacy compacted summary".to_string(),
                replacement_history: None,
                retained_context: None,
                guardian_history: None,
                mcp_resource_origins: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                compaction_response_id: None,
                latest_token_usage_record: None,
                resume_metadata: Some(codex_history::CompactionResumeMetadata {
                    multi_agent_version: Some(MultiAgentVersion::V2),
                    last_started_turn_id: None,
                    turn_attribution: None,
                    previous_turn_settings: Some(codex_history::PreviousTurnSettings {
                        model: "parent-model".into(),
                        comp_hash: None,
                        cyber_access_program: None,
                        realtime_active: None,
                    }),
                }),
            })),
        ];
        if let Some(instructions) = parent_developer_instructions {
            rollout_items.push(rollout_response_item(ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: instructions.to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }));
        }
        rollout_items.push(rollout_response_item(ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: stale_managed_fragment.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }));
        rollout_items.extend([
            rollout_response_item(ContextualUserFragment::into(BaseInstructionsFragment(
                "Original model instructions.".to_string(),
            ))),
            rollout_response_item(ContextualUserFragment::into(ModelInstructions {
                model: None,
                instructions: Some("Updated model instructions.".to_string()),
            })),
        ]);
        rollout_items.push(rollout_response_item(ContextualUserFragment::into(
            IncrementalToolsHint(
                codex_prompts::ResolvedIncrementalToolMessages::default().tool_update_hint,
            ),
        )));
        rollout_items.push(rollout_response_item(ResponseItem::AdditionalTools {
            id: None,
            role: "developer".to_string(),
            tools: Arc::from([serde_json::json!({"type": "function", "name": "parent_tool"})]),
        }));
        rollout_items.push(RolloutItem::TurnContext(
            turn_context.to_turn_context_item(),
        ));
        rollout_items.push(rollout_response_item(spawn_agent_call(
            parent_spawn_call_id,
        )));
        parent_thread
            .session
            .persist_rollout_items(&rollout_items)
            .await;
        parent_thread
            .session
            .ensure_rollout_materialized(PersistContext::Standard)
            .await;
        parent_thread
            .session
            .flush_rollout()
            .await
            .expect("parent rollout should flush");

        let child_thread_id = harness
            .control
            .spawn_agent_with_metadata(
                child_config,
                text_input("child task"),
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                }),
                SpawnAgentOptions {
                    fork_parent_spawn_call_id: Some(parent_spawn_call_id.to_string()),
                    fork_mode: Some(SpawnAgentForkMode::FullHistory),
                    ..Default::default()
                },
            )
            .await
            .expect("forked spawn should preserve legacy compacted history")
            .thread_id;
        let child_thread = harness
            .manager
            .get_thread(child_thread_id)
            .await
            .expect("child thread should be registered");
        child_thread
            .flush_rollout()
            .await
            .expect("flush child checkpoint");
        let (items, _, _) = codex_rollout::RolloutRecorder::load_rollout_items(
            &child_thread.rollout_path().expect("child rollout"),
        )
        .await
        .expect("read child checkpoint");
        assert_eq!(
            items
                .iter()
                .filter_map(|item| match item {
                    RolloutItem::ResponseItem(envelope)
                        if matches!(&envelope.item, ResponseItem::Message {
                            internal_chat_message_metadata_passthrough: Some(metadata), ..
                        } if metadata.content_item_kinds.as_ref().is_some_and(|kinds| {
                            kinds.iter().any(|kind| kind.as_str() == IncrementalToolsHint::KIND)
                        })) =>
                        Some(envelope.item.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            Vec::<ResponseItem>::new(),
            "{case}: a legacy compaction fork must drop notices for tool declarations it rebuilds"
        );
        let inherited_state = items.into_iter().find_map(|item| match item {
            RolloutItem::Compacted(item) => item.resume_metadata,
            _ => None,
        });
        assert_eq!(
            inherited_state,
            Some(codex_history::CompactionResumeMetadata {
                multi_agent_version: Some(MultiAgentVersion::V2),
                last_started_turn_id: None,
                turn_attribution: None,
                previous_turn_settings: None,
            })
        );
        while child_thread
            .session
            .reference_context_item()
            .await
            .is_none()
        {
            tokio::task::yield_now().await;
        }
        let history = child_thread.session.clone_history().await;
        let mut instruction_count = 0;
        let mut managed_instructions = Vec::new();
        let mut model_instructions = Vec::new();
        for item in history.raw_items() {
            if let Some(base) = BaseInstructionsFragment::text_from_item(item) {
                model_instructions.push(base);
            }
            let ResponseItem::Message { role, content, .. } = item else {
                continue;
            };
            if role != "developer" {
                continue;
            }
            for content_item in content {
                if let ContentItem::InputText { text } = content_item {
                    instruction_count += usize::from(text == "Child developer instructions.");
                    if ModelInstructions::matches_text(text) {
                        model_instructions.push(text.as_str());
                    }
                    if ManagedDeveloperInstructions::matches_text(text) {
                        managed_instructions.push(text.as_str());
                    }
                }
            }
        }
        assert_eq!(
            (instruction_count, managed_instructions, model_instructions),
            (
                1,
                vec![current_managed_fragment.as_str()],
                vec!["Current model instructions."],
            ),
            "{case}: canonical context reconstruction must keep only the current child, managed, and model instructions"
        );

        let _ = harness
            .control
            .shutdown_live_agent(child_thread_id)
            .await
            .expect("child shutdown should submit");
        let _ = parent_thread
            .submit(Op::Shutdown {})
            .await
            .expect("parent shutdown should submit");
    }
}

#[test_case::test_case(false; "complete")]
#[test_case::test_case(true; "error_reason")]
#[tokio::test]
async fn resume_and_cold_fork_restore_mcp_attribution_in_constructed_requests(
    record_invalid_source: bool,
) {
    let harness = AgentControlHarness::new().await;
    let source = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start legacy source");
    let source_thread_id = source.thread_id;
    let source_thread = source.thread;
    let turn_context = source_thread.session.new_default_turn().await;
    source_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            turn_context.model_info(),
            &[user_message("source turn")],
        )
        .await;
    let source = McpAttributionSource {
        connector_id: None,
        plugin_id: Some("messages@openai-bundled".to_string()),
        server_name: "messages".to_string(),
        tool_name: "search".to_string(),
        first_turn_id: turn_context.sub_id.clone(),
    };
    source_thread
        .session
        .services
        .executed_tool_calls
        .record_mcp_source(source.clone());
    if record_invalid_source {
        source_thread
            .session
            .services
            .executed_tool_calls
            .record_mcp_source(McpAttributionSource {
                tool_name: String::new(),
                ..source.clone()
            });
    }
    source_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            turn_context.model_info(),
            &[assistant_message("source result", /*phase*/ None)],
        )
        .await;
    source_thread.ensure_rollout_materialized().await;
    source_thread
        .flush_rollout()
        .await
        .expect("flush source rollout");
    let rollout_path = source_thread.rollout_path().expect("source rollout path");
    source_thread
        .shutdown_and_wait()
        .await
        .expect("shutdown source thread");
    assert!(
        harness
            .manager
            .remove_thread(&source_thread_id)
            .await
            .is_some()
    );
    drop(source_thread);

    let forked = harness
        .manager
        .fork_legacy_thread(
            ForkSnapshot::Interrupted,
            StartThreadOptions::new(harness.config.clone()),
            rollout_path.clone(),
        )
        .await
        .expect("fork unloaded source thread");
    let resumed = harness
        .manager
        .resume_legacy_thread_from_rollout(
            harness.config.clone(),
            rollout_path,
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy")),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await
        .expect("resume source thread");
    let expected = McpAttribution {
        status: if record_invalid_source {
            McpAttributionStatus::AttributionError
        } else {
            McpAttributionStatus::Complete
        },
        error_reason: record_invalid_source.then_some(McpAttributionErrorReason::SourceInvalid),
        sources: vec![source],
    };
    assert_eq!(
        mcp_attribution_in_constructed_request(&forked.thread).await,
        expected
    );
    assert_eq!(
        mcp_attribution_in_constructed_request(&resumed.thread).await,
        expected
    );

    forked
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown forked thread");
    resumed
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown resumed thread");
}

#[tokio::test]
async fn spawn_agent_fork_flushes_parent_rollout_before_loading_history() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-unflushed".to_string();
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            turn_context.model_info(),
            &[
                assistant_message("unflushed final answer", Some(MessagePhase::FinalAnswer)),
                spawn_agent_call(&parent_spawn_call_id),
            ],
        )
        .await;

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("child task"),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should flush parent rollout before loading history")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(history.raw_items(), "unflushed final answer"),
        "forked child history should include unflushed assistant final answers after flushing the parent rollout"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_respects_legacy_max_threads_alias() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_threads".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let _ = manager
        .start_thread_with_control_for_tests(
            StartThreadOptions::new(config.clone()),
            control.clone(),
        )
        .await
        .expect("start thread");

    let first_agent_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");

    let err = control
        .spawn_agent(
            config,
            text_input("hello again"),
            /*session_source*/ None,
        )
        .await
        .expect_err("spawn_agent should respect max threads");
    let CodexErrorDetails::AgentLimitReached {
        max_threads: seen_max_threads,
    } = err.details()
    else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*seen_max_threads, max_threads);

    let _ = control
        .shutdown_live_agent(first_agent_id)
        .await
        .expect("shutdown agent");
}

#[tokio::test]
async fn spawn_agent_releases_slot_after_shutdown() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let first_agent_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");
    let _ = control
        .shutdown_live_agent(first_agent_id)
        .await
        .expect("shutdown agent");

    let second_agent_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello again"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed after shutdown");
    let _ = control
        .shutdown_live_agent(second_agent_id)
        .await
        .expect("shutdown agent");
}

#[tokio::test]
async fn spawn_agent_limit_shared_across_clones() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();
    let cloned = control.clone();

    let first_agent_id = cloned
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");

    let err = control
        .spawn_agent(
            config,
            text_input("hello again"),
            /*session_source*/ None,
        )
        .await
        .expect_err("spawn_agent should respect shared guard");
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 1);

    let _ = control
        .shutdown_live_agent(first_agent_id)
        .await
        .expect("shutdown agent");
}

#[tokio::test]
async fn resume_agent_respects_max_threads_limit() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let resumable_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");
    let saved_source = manager
        .get_thread(resumable_id)
        .await
        .unwrap()
        .session_source
        .clone();
    let _ = control
        .shutdown_live_agent(resumable_id)
        .await
        .expect("shutdown resumable thread");

    let active_id = control
        .spawn_agent(
            config.clone(),
            text_input("occupy"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed for active slot");

    let err = control
        .resume_agent_from_rollout(config, resumable_id, saved_source)
        .await
        .expect_err("resume should respect max threads");
    let CodexErrorDetails::AgentLimitReached {
        max_threads: seen_max_threads,
    } = err.details()
    else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*seen_max_threads, max_threads);

    let _ = control
        .shutdown_live_agent(active_id)
        .await
        .expect("shutdown active thread");
}

#[tokio::test]
async fn resume_agent_releases_slot_after_resume_failure() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let _ = control
        .resume_agent_from_rollout(config.clone(), ThreadId::new(), SessionSource::Exec)
        .await
        .expect_err("resume should fail for missing rollout path");

    let resumed_id = control
        .spawn_agent(config, text_input("hello"), /*session_source*/ None)
        .await
        .expect("spawn should succeed after failed resume");
    let _ = control
        .shutdown_live_agent(resumed_id)
        .await
        .expect("shutdown resumed thread");
}

#[tokio::test]
async fn spawn_child_completion_notifies_parent_history() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let _ = child_thread
        .submit(Op::Shutdown {})
        .await
        .expect("child shutdown should submit");

    assert_eq!(wait_for_subagent_notification(&parent_thread).await, true);
}

#[tokio::test]
async fn multi_agent_v2_completion_ignores_dead_direct_parent() {
    let harness = AgentControlHarness::new().await;
    let mut config = harness.config.clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    let root = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions::new(config.clone()),
            harness.control.clone(),
        )
        .await
        .expect("root thread should start");
    let root_thread_id = root.thread_id;
    let root_thread = root.thread;
    let worker_path = AgentPath::root().join("worker_a").expect("worker path");
    let worker_thread_id = harness
        .control
        .spawn_agent(
            config.clone(),
            text_input("hello worker"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_thread_id,
                depth: 1,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("worker spawn should succeed");
    let tester_path = worker_path.join("tester").expect("tester path");
    let tester_thread_id = harness
        .control
        .spawn_agent(
            config,
            text_input("hello tester"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: Some(tester_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("tester spawn should succeed");
    harness
        .control
        .shutdown_live_agent(worker_thread_id)
        .await
        .expect("worker shutdown should succeed");

    let tester_thread = harness
        .manager
        .get_thread(tester_thread_id)
        .await
        .expect("tester thread should exist");
    let tester_turn = tester_thread.session.new_default_turn().await;
    tester_thread
        .session
        .send_event(
            tester_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                root_turn_id: None,
                turn_id: tester_turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("done".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    sleep(Duration::from_millis(100)).await;

    assert!(
        !harness
            .manager
            .captured_ops()
            .into_iter()
            .any(|(thread_id, op)| {
                thread_id == worker_thread_id
                    && matches!(
                        op,
                        Op::InterAgentCommunication { communication, .. }
                            if communication.author == tester_path
                                && communication.recipient == worker_path
                                && communication.content == "done"
                    )
            })
    );

    let root_history = root_thread.session.clone_history().await;
    assert!(!history_contains_assistant_inter_agent_communication(
        root_history.raw_items(),
        &InterAgentCommunication::new(
            tester_path,
            AgentPath::root(),
            Vec::new(),
            "done".to_string(),
            /*trigger_turn*/ true,
        )
    ));
    assert!(!has_subagent_notification(root_history.raw_items()));
}

#[tokio::test]
async fn multi_agent_v2_completion_queues_message_for_direct_parent() {
    let harness = AgentControlHarness::new().await;
    let (_root_thread_id, root_thread) = harness.start_thread().await;
    let (worker_thread_id, _worker_thread) = harness.start_thread().await;
    let mut tester_config = harness.config.clone();
    let _ = tester_config.features.enable(Feature::MultiAgentV2);
    let tester_thread_id = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions::new(tester_config.clone()),
            harness.control.clone(),
        )
        .await
        .expect("tester thread should start")
        .thread_id;
    let tester_thread = harness
        .manager
        .get_thread(tester_thread_id)
        .await
        .expect("tester thread should exist");
    let worker_path = AgentPath::root().join("worker_a").expect("worker path");
    let tester_path = worker_path.join("tester").expect("tester path");
    harness.control.maybe_start_completion_watcher(
        tester_thread_id,
        Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: worker_thread_id,
            depth: 2,
            agent_path: Some(tester_path.clone()),
            agent_nickname: None,
            agent_role: Some("explorer".to_string()),
        })),
        tester_path.to_string(),
        Some(tester_path.clone()),
    );
    let tester_turn = tester_thread.session.new_default_turn().await;
    tester_thread
        .session
        .send_event(
            tester_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                root_turn_id: None,
                turn_id: tester_turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("done".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    let expected_message = crate::session_prefix::format_inter_agent_completion_message(
        worker_path.clone(),
        tester_path.clone(),
        &AgentStatus::Completed(Some("done".to_string())),
    )
    .expect("completed status should render");
    let expected = (
        worker_thread_id,
        Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                tester_path.clone(),
                worker_path.clone(),
                Vec::new(),
                expected_message.clone(),
                /*trigger_turn*/ false,
            ),
            start_options: Default::default(),
        },
    );

    timeout(Duration::from_secs(5), async {
        loop {
            let captured = harness
                .manager
                .captured_ops()
                .into_iter()
                .find(|entry| captured_op_matches(entry, &expected));
            if captured.is_some() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("completion watcher should queue a direct-parent message");

    let root_history = root_thread.session.clone_history().await;
    assert!(!history_contains_assistant_inter_agent_communication(
        root_history.raw_items(),
        &InterAgentCommunication::new(
            tester_path,
            AgentPath::root(),
            Vec::new(),
            expected_message,
            /*trigger_turn*/ false,
        )
    ));
}

#[tokio::test]
async fn completion_watcher_notifies_parent_when_child_is_missing() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_thread_id = ThreadId::new();

    harness.control.maybe_start_completion_watcher(
        child_thread_id,
        Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("explorer".to_string()),
        })),
        child_thread_id.to_string(),
        /*child_agent_path*/ None,
    );

    assert_eq!(wait_for_subagent_notification(&parent_thread).await, true);

    let history = parent_thread.session.clone_history().await;
    assert_eq!(
        history_contains_text(
            history.raw_items(),
            &format!("\"agent_path\":\"{child_thread_id}\"")
        ),
        true
    );
    assert_eq!(
        history_contains_text(history.raw_items(), "\"status\":\"not_found\""),
        true
    );
}

#[tokio::test]
async fn completion_watcher_does_not_hide_tree_shutdown_failure() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;
    let (child_thread_id, _child_thread) = harness.start_thread().await;

    harness.control.maybe_start_completion_watcher(
        child_thread_id,
        Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("explorer".to_string()),
        })),
        child_thread_id.to_string(),
        /*child_agent_path*/ None,
    );
    harness.control.runtime.record_shutdown_failure(
        crate::thread_manager::AgentTreeShutdownFailure::operation_failed(
            "completion_watcher_test",
            "test",
            Some(parent_thread_id),
            "test_error",
        ),
    );
    let shutdown = harness.control.runtime.request_shutdown();

    timeout(Duration::from_secs(5), shutdown.wait())
        .await
        .expect("completion watcher should stop during tree shutdown")
        .expect_err("recorded tree shutdown failure should be returned");
}

#[tokio::test]
async fn spawn_thread_subagent_gets_random_nickname_in_session_source() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let snapshot = child_thread.config_snapshot().await;

    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: seen_parent_thread_id,
        depth,
        agent_nickname,
        agent_role,
        ..
    }) = snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(seen_parent_thread_id, parent_thread_id);
    assert_eq!(depth, 1);
    assert!(agent_nickname.is_some());
    assert_eq!(agent_role, Some("explorer".to_string()));
}

#[tokio::test]
async fn spawn_thread_subagents_persist_parent_originator_across_new_and_full_fork() {
    let harness = AgentControlHarness::new().await;
    let parent = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                metrics_service_name: Some("codex_work_desktop".to_string()),
                environments: Some(Vec::new()),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("parent thread should start");
    let parent_originator = persisted_originator(&parent.thread).await;
    assert_eq!(parent_originator, "codex_work_desktop");

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: parent.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let child_originator = persisted_originator(&child_thread).await;
    assert_eq!(child_originator, parent_originator);

    let child = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("hello forked child"),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: parent.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some("spawn-call-full-history".to_string()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child.thread_id)
        .await
        .expect("child thread should be registered");
    let child_originator = persisted_originator(&child_thread).await;
    assert_eq!(child_originator, parent_originator);
}

#[tokio::test]
async fn spawn_thread_subagent_uses_role_specific_nickname_candidates() {
    let mut harness = AgentControlHarness::new().await;
    harness.config.agent_roles.insert(
        "researcher".to_string(),
        AgentRoleConfig {
            description: Some("Research role".to_string()),
            config_file: None,
            nickname_candidates: Some(vec!["Atlas".to_string()]),
        },
    );
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("researcher".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let snapshot = child_thread.config_snapshot().await;

    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_nickname, .. }) =
        snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(agent_nickname, Some("Atlas".to_string()));
}

/// Resume restores the child's saved metadata and roots even after its parent deselects an environment.
#[tokio::test]
async fn resume_thread_subagent_restores_stored_metadata() {
    let (home, config) = test_config().await;
    let thread_store = Arc::new(InMemoryThreadStore::default());
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth_manager.clone(),
        crate::thread_manager::build_models_manager(&config, auth_manager),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        thread_store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let roots = vec![SelectedCapabilityRoot {
        id: "saved-root".to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
            path: PathUri::from_abs_path(&harness.config.cwd),
        },
    }];
    let mut thread_extension_init = ExtensionDataInit::new();
    thread_extension_init.insert(roots.clone());
    let parent = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                thread_extension_init,
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .expect("start parent with roots");
    let parent_thread_id = parent.thread_id;
    let parent_thread = parent.thread;
    let agent_path = AgentPath::from_string("/root/explorer".to_string())
        .expect("test agent path should be valid");

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(agent_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    child_thread
        .session
        .ensure_rollout_materialized(PersistContext::Standard)
        .await;
    child_thread
        .session
        .flush_rollout()
        .await
        .expect("flush child rollout");
    let mut status_rx = child_thread.subscribe_status();
    if matches!(status_rx.borrow().clone(), AgentStatus::PendingInit) {
        timeout(Duration::from_secs(5), async {
            loop {
                status_rx
                    .changed()
                    .await
                    .expect("child status should advance past pending init");
                if !matches!(status_rx.borrow().clone(), AgentStatus::PendingInit) {
                    break;
                }
            }
        })
        .await
        .expect("child should initialize before shutdown");
    }
    let original_environments = child_thread.environment_selections().await;
    assert_eq!(
        child_thread.session.services.selected_capability_roots,
        roots
    );
    let original_snapshot = child_thread.config_snapshot().await;
    let original_nickname = original_snapshot
        .session_source
        .get_nickname()
        .expect("spawned sub-agent should have a nickname");
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(stored_thread) = thread_store
                .read_thread(ReadThreadParams {
                    thread_id: child_thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
                && stored_thread.agent_nickname.is_some()
                && stored_thread.agent_role.as_deref() == Some("explorer")
                && stored_thread.agent_path.as_deref() == Some(agent_path.as_str())
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child thread metadata should be persisted to sqlite before shutdown");

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");

    parent_thread
        .update_thread_settings(ThreadSettingsOverrides {
            environments: Some(TurnEnvironmentRequests::new(
                harness.config.cwd.clone(),
                Vec::new(),
            )),
            ..Default::default()
        })
        .await
        .expect("deselect parent environment");
    // Resume inherits the parent's current attachments, so activate the accepted settings.
    let parent_turn = parent_thread
        .session
        .new_turn_with_default_settings("parent-deselected".to_string(), Default::default())
        .await;
    assert_eq!(
        parent_turn.initial_environments.selected_capability_roots(),
        Vec::new()
    );

    let resumed_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            child_thread_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
        )
        .await
        .expect("resume should succeed");
    assert_eq!(resumed_thread_id, child_thread_id);

    let resumed_thread = harness
        .manager
        .get_thread(resumed_thread_id)
        .await
        .expect("resumed child thread should exist");
    assert_eq!(
        resumed_thread.session.services.selected_capability_roots,
        roots
    );
    resumed_thread
        .update_thread_settings(ThreadSettingsOverrides {
            environments: Some(TurnEnvironmentRequests::new(
                harness.config.cwd.clone(),
                original_environments
                    .clone()
                    .into_iter()
                    .map(TurnEnvironmentSelection::into_request)
                    .collect(),
            )),
            ..Default::default()
        })
        .await
        .expect("reselect child environment");
    assert_eq!(
        resumed_thread.environment_selections().await,
        original_environments
    );
    let child_turn = resumed_thread
        .session
        .new_turn_with_default_settings("child-reselected".to_string(), Default::default())
        .await;
    assert_eq!(
        child_turn.initial_environments.selected_capability_roots(),
        roots
    );
    let resumed_snapshot = resumed_thread.config_snapshot().await;
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: resumed_parent_thread_id,
        depth: resumed_depth,
        agent_path: resumed_agent_path,
        agent_nickname: resumed_nickname,
        agent_role: resumed_role,
        ..
    }) = resumed_snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_eq!(resumed_depth, 1);
    assert_eq!(resumed_agent_path, Some(agent_path));
    assert_eq!(resumed_nickname, Some(original_nickname));
    assert_eq!(resumed_role, Some("explorer".to_string()));

    let _ = harness
        .control
        .shutdown_live_agent(resumed_thread_id)
        .await
        .expect("resumed child shutdown should submit");
}

#[tokio::test]
async fn resume_agent_from_rollout_reads_archived_rollout_path() {
    let harness = AgentControlHarness::new().await;
    let (child_thread_id, child_thread) = harness.start_thread().await;
    persist_thread_for_tree_resume(&child_thread, "persist before archiving").await;
    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should succeed");
    let store = LocalThreadStore::new(
        LocalThreadStoreConfig::from_config(&harness.config),
        harness.state_db.clone(),
    );
    store
        .archive_thread(ArchiveThreadParams {
            thread_id: child_thread_id,
        })
        .await
        .expect("child thread should archive");

    let resumed_thread_id = harness
        .control
        .resume_agent_from_rollout(harness.config.clone(), child_thread_id, SessionSource::Exec)
        .await
        .expect("resume should find archived rollout");
    assert_eq!(resumed_thread_id, child_thread_id);

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("resumed child shutdown should succeed");
}

#[test_case::test_case(vec!["child-plugin".to_string()]; "saved disabled plugin")]
#[test_case::test_case(Vec::new(); "saved empty selection")]
#[tokio::test]
async fn resume_agent_from_paginated_rollout_loads_model_context(disabled_plugin_ids: Vec<String>) {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_paginated_thread().await;
    let child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                disabled_plugin_ids: Some(disabled_plugin_ids.clone()),
                ..Default::default()
            },
        )
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    assert_eq!(
        child_thread.config_snapshot().await.history_mode,
        ThreadHistoryMode::Paginated
    );
    persist_thread_for_tree_resume(&child_thread, "persist before resume").await;
    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should succeed");

    // Resuming restores the child's saved selection even after its parent changes.
    parent_thread
        .update_thread_settings(ThreadSettingsOverrides {
            disabled_plugin_ids: Some(vec!["parent-plugin".to_string()]),
            ..Default::default()
        })
        .await
        .expect("change parent plugin selection");

    let resumed_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            child_thread_id,
            child_thread.session_source.clone(),
        )
        .await
        .expect("resume should load paginated model context");
    assert_eq!(resumed_thread_id, child_thread_id);
    let resumed_thread = harness
        .manager
        .get_thread(resumed_thread_id)
        .await
        .expect("resumed child thread should exist");
    assert_eq!(
        resumed_thread
            .thread_settings_snapshot()
            .await
            .disabled_plugin_ids,
        disabled_plugin_ids
    );
    assert!(
        history_contains_text(
            resumed_thread.session.clone_history().await.raw_items(),
            "persist before resume",
        ),
        "resumed child should keep its persisted model context"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("resumed child shutdown should succeed");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn list_agent_subtree_thread_ids_includes_anonymous_and_closed_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;
    let worker_path = AgentPath::root().join("worker").expect("worker path");
    let reviewer_path = AgentPath::root().join("reviewer").expect("reviewer path");

    let worker_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello worker"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("worker spawn should succeed");
    let worker_child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello worker child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: Some(
                    worker_path
                        .join("child")
                        .expect("worker child path should be valid"),
                ),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("worker child spawn should succeed");
    let no_path_child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello anonymous child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("no-path child spawn should succeed");
    let no_path_grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello anonymous grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: no_path_child_thread_id,
                depth: 3,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("no-path grandchild spawn should succeed");
    let _reviewer_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello reviewer"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(reviewer_path),
                agent_nickname: None,
                agent_role: Some("reviewer".to_string()),
            })),
        )
        .await
        .expect("reviewer spawn should succeed");

    let _ = harness
        .control
        .shutdown_live_agent(no_path_grandchild_thread_id)
        .await
        .expect("no-path grandchild shutdown should succeed");

    let mut worker_subtree_thread_ids = harness
        .manager
        .list_agent_subtree_thread_ids(worker_thread_id)
        .await
        .expect("worker subtree thread ids should load");
    worker_subtree_thread_ids.sort_by_key(ToString::to_string);
    let mut expected_worker_subtree_thread_ids = vec![
        worker_thread_id,
        worker_child_thread_id,
        no_path_child_thread_id,
        no_path_grandchild_thread_id,
    ];
    expected_worker_subtree_thread_ids.sort_by_key(ToString::to_string);
    assert_eq!(
        worker_subtree_thread_ids,
        expected_worker_subtree_thread_ids
    );

    let mut no_path_child_subtree_thread_ids = harness
        .manager
        .list_agent_subtree_thread_ids(no_path_child_thread_id)
        .await
        .expect("no-path subtree thread ids should load");
    no_path_child_subtree_thread_ids.sort_by_key(ToString::to_string);
    let mut expected_no_path_child_subtree_thread_ids =
        vec![no_path_child_thread_id, no_path_grandchild_thread_id];
    expected_no_path_child_subtree_thread_ids.sort_by_key(ToString::to_string);
    assert_eq!(
        no_path_child_subtree_thread_ids,
        expected_no_path_child_subtree_thread_ids
    );
}

#[tokio::test]
async fn list_agent_subtree_thread_ids_finds_live_descendants_of_unloaded_root() {
    let (_home, config) = test_config().await;
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        /*state_db*/ None,
    );
    let control = manager.agent_control();
    let parent_thread_id = manager
        .start_thread_with_control_for_tests(
            StartThreadOptions::new(config.clone()),
            control.clone(),
        )
        .await
        .expect("parent should start")
        .thread_id;

    let child_thread_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = control
        .spawn_agent(
            config,
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    manager.remove_thread(&parent_thread_id).await;

    let mut subtree_thread_ids = manager
        .list_agent_subtree_thread_ids(parent_thread_id)
        .await
        .expect("live subtree should load");
    subtree_thread_ids.sort_by_key(ToString::to_string);
    let mut expected_subtree_thread_ids =
        vec![parent_thread_id, child_thread_id, grandchild_thread_id];
    expected_subtree_thread_ids.sort_by_key(ToString::to_string);

    assert_eq!(subtree_thread_ids, expected_subtree_thread_ids);
}

#[tokio::test]
async fn shutdown_agent_tree_closes_live_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown should succeed");

    assert_eq!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let shutdown_ids = harness
        .manager
        .captured_ops()
        .into_iter()
        .filter_map(|(thread_id, op)| matches!(op, Op::Shutdown).then_some(thread_id))
        .collect::<Vec<_>>();
    let mut expected_shutdown_ids = vec![parent_thread_id, child_thread_id, grandchild_thread_id];
    expected_shutdown_ids.sort_by_key(std::string::ToString::to_string);
    let mut shutdown_ids = shutdown_ids;
    shutdown_ids.sort_by_key(std::string::ToString::to_string);
    assert_eq!(shutdown_ids, expected_shutdown_ids);
}

#[tokio::test]
async fn shutdown_agent_tree_closes_descendants_when_started_at_child() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown should succeed");

    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );

    let shutdown_ids = harness
        .manager
        .captured_ops()
        .into_iter()
        .filter_map(|(thread_id, op)| matches!(op, Op::Shutdown).then_some(thread_id))
        .collect::<Vec<_>>();
    let mut expected_shutdown_ids = vec![parent_thread_id, child_thread_id, grandchild_thread_id];
    expected_shutdown_ids.sort_by_key(std::string::ToString::to_string);
    let mut shutdown_ids = shutdown_ids;
    shutdown_ids.sort_by_key(std::string::ToString::to_string);
    assert_eq!(shutdown_ids, expected_shutdown_ids);
}

#[tokio::test]
async fn resume_agent_from_rollout_does_not_reopen_closed_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");
    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
        )
        .await
        .expect("single-thread resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown after resume should succeed");
}

#[tokio::test]
async fn resume_closed_child_reopens_open_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");

    let resumed_child_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            child_thread_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
        )
        .await
        .expect("child resume should succeed");
    assert_eq!(resumed_child_thread_id, child_thread_id);
    assert_ne!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close after resume should succeed");
    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
}

#[tokio::test]
async fn resume_agent_from_rollout_reopens_open_descendants_after_manager_shutdown() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
        )
        .await
        .expect("tree resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown after subtree resume should succeed");
}

#[tokio::test]
async fn resume_agent_from_rollout_uses_edge_data_when_descendant_metadata_source_is_stale() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let state_db = grandchild_thread
        .state_db()
        .expect("sqlite state db should be available");
    let mut stale_metadata = state_db
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild metadata query should succeed")
        .expect("grandchild metadata should exist");
    stale_metadata.source =
        serde_json::to_string(&SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 99,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("worker".to_string()),
        }))
        .expect("stale session source should serialize");
    state_db
        .upsert_thread(&stale_metadata)
        .await
        .expect("stale grandchild metadata should persist");

    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
        )
        .await
        .expect("tree resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let resumed_grandchild_snapshot = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("resumed grandchild thread should exist")
        .config_snapshot()
        .await;
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: resumed_parent_thread_id,
        depth: resumed_depth,
        ..
    }) = resumed_grandchild_snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(resumed_parent_thread_id, child_thread_id);
    assert_eq!(resumed_depth, 2);

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown after subtree resume should succeed");
}

#[tokio::test]
async fn resume_agent_from_rollout_skips_descendants_when_parent_resume_fails() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let child_rollout_path = child_thread
        .rollout_path()
        .expect("child thread should have rollout path");
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());
    tokio::fs::remove_file(&child_rollout_path)
        .await
        .expect("child rollout path should be removable");

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
        )
        .await
        .expect("root resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown after partial subtree resume should succeed");
}

#[tokio::test]
async fn saved_child_reload_rejects_missing_private_permit_and_forged_caller_without_capacity_loss()
{
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::Sqlite).unwrap();
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_paginated_thread().await;
    let control = parent
        .session
        .services
        .local_agent_runtime
        .control(parent.session.session_id());
    let child =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &parent, "worker").await;
    let thread = harness.manager.get_thread(child.thread_id).await.unwrap();
    persist_thread_for_tree_resume(&thread, "saved child security fixture").await;
    let stored = thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ false,
        )
        .await
        .unwrap();
    let state = control.runtime.upgrade().unwrap();
    let context = state
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: child.thread_id,
            include_archived: true,
        })
        .await
        .unwrap();
    let history = Arc::new(context.items);
    let source = thread.session_source.clone();
    assert_eq!(
        harness.manager.try_evict_v2_thread(thread).await.unwrap(),
        ThreadEvictionOutcome::Evicted
    );
    for requested_source in [source, SessionSource::Exec] {
        let error = state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                authorization: None,
                config: harness.config.clone(),
                prompt_cache_key: None,
                initial_history: InitialHistory::Resumed(ResumedHistory {
                    history_revision: None,
                    conversation_id: child.thread_id,
                    history: Arc::clone(&history),
                    rollout_path: stored.rollout_path.clone(),
                }),
                agent_control: control.clone(),
                session_source: requested_source,
                parent_thread_id: Some(parent_id),
                environment_selections: None,
                inherited_environments: None,
                inherited_instructions: None,
                inherited_exec_policy: None,
                client_mcp_extensions: None,
            })
            .await
            .err()
            .expect("raw saved history/source cannot classify a trusted child launch");
        assert!(
            error
                .to_string()
                .contains("private host resume authorization"),
            "{error}"
        );
        assert_thread_not_loaded(&harness.manager, child.thread_id).await;
        assert!(harness.manager.get_thread(parent_id).await.is_ok());
    }
    let absent_caller = ThreadId::new();
    let error = match control
        .ensure_v2_agent_loaded(
            harness.config.clone(),
            child.thread_id,
            absent_caller,
            /*parent*/ None,
        )
        .await
    {
        Ok(_) => panic!("invented sender must fail before reservation"),
        Err(error) => error,
    };
    assert!(
        matches!(error.details(), CodexErrorDetails::ThreadNotFound(id) if *id == absent_caller)
    );
    assert_thread_not_loaded(&harness.manager, child.thread_id).await;
    let pin = control
        .ensure_v2_agent_loaded(
            harness.config.clone(),
            child.thread_id,
            parent_id,
            /*parent*/ None,
        )
        .await
        .expect("real caller reload uses the one available child slot");
    assert!(harness.manager.get_thread(parent_id).await.is_ok());
    assert!(harness.manager.get_thread(child.thread_id).await.is_ok());
    drop(pin);
}

#[test_case::test_case(false, false; "removed custom default")]
#[test_case::test_case(true, false; "default grandchild under restrictive ancestor")]
#[test_case::test_case(false, true; "cold executor environment access cannot widen")]
#[tokio::test]
async fn cold_reload_cannot_forget_a_sealed_effective_role_ceiling(
    nested: bool,
    environment_access: bool,
) {
    let role = if nested { "restricted" } else { "default" };
    let host = if environment_access {
        "[shell_environment_policy]\ninherit='none'\n[agents]\nallowed_model_providers=['openai']\n"
            .to_owned()
    } else {
        format!(
            "[agents]\nallowed_model_providers=['openai']\n[agents.{role}]\nconfig_file='restricted.toml'\ndescription='Restricted authority fixture'\n"
        )
    };
    let (home, _initial_config) =
        test_config_with_host_configuration(Vec::new(), Some(&host)).await;
    std::fs::write(home.path().join("restricted.toml"), "[features]\nshell_tool=false\n[skills]\ninclude_instructions=false\nbundled={enabled=false}\n").unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .cli_overrides(vec![("agents.max_depth".into(), TomlValue::Integer(8))])
        .build()
        .await
        .unwrap();
    assert!(
        config.agent_roles.contains_key(role) != environment_access,
        "host role authority must match the fixture before runtime startup"
    );
    for feature in [Feature::Sqlite, Feature::MultiAgentV2, Feature::ShellTool] {
        config.features.enable(feature).unwrap();
    }
    config.model = Some("gpt-5.6-sol".into());
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let root = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .unwrap();
    let control = root
        .thread
        .session
        .services
        .local_agent_runtime
        .control(root.thread.session.session_id());
    let source = thread_spawn_source(
        root.thread_id,
        &root.thread.session_source,
        /*depth*/ 1,
        nested.then_some("restricted"),
        Some("worker".into()),
    )
    .unwrap();
    let worker = control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("worker"),
            source,
            SpawnAgentOptions {
                parent_thread_id: Some(root.thread_id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let worker_thread = harness.manager.get_thread(worker.thread_id).await.unwrap();
    let target = if nested {
        spawn_v2_reload_test_child(&control, harness.config.clone(), &worker_thread, "child")
            .await
            .thread_id
    } else {
        worker.thread_id
    };
    let target_thread = harness.manager.get_thread(target).await.unwrap();
    let old = target_thread.session.get_config().await;
    assert_eq!(old.features.enabled(Feature::ShellTool), environment_access);
    assert_eq!(old.include_skill_instructions, environment_access);
    assert_eq!(old.bundled_skills_enabled(), environment_access);
    if environment_access {
        assert_eq!(
            old.permissions.shell_environment_policy.inherit,
            codex_protocol::config_types::ShellEnvironmentPolicyInherit::None
        );
        let actual = target_thread.session.inherited_environment_snapshot().await;
        assert_eq!(
            actual
                .primary()
                .unwrap()
                .config()
                .shell_environment_policy
                .inherit,
            codex_protocol::config_types::ShellEnvironmentPolicyInherit::None
        );
    }
    persist_thread_for_tree_resume(&root.thread, "root receipt fixture").await;
    persist_thread_for_tree_resume(&worker_thread, "worker receipt fixture").await;
    if nested {
        persist_thread_for_tree_resume(&target_thread, "target receipt fixture").await;
    }
    wait_for_live_thread_spawn_children(&control, root.thread_id, &[worker.thread_id]).await;
    if nested {
        wait_for_live_thread_spawn_children(&control, worker.thread_id, &[target]).await;
    }
    let stored = root
        .thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .unwrap();
    let initial = InitialHistory::Resumed(ResumedHistory {
        history_revision: None,
        conversation_id: root.thread_id,
        history: Arc::new(stored.history.unwrap().items),
        rollout_path: stored.rollout_path,
    });
    let state = control.runtime.upgrade().unwrap();
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert!(report.timed_out.is_empty());
    assert!(
        state
            .read_native_authority_receipt(target)
            .await
            .unwrap()
            .is_some(),
        "actual clean shutdown must seal the restrictive target"
    );
    std::fs::write(
        harness.config.codex_home.join("config.toml"),
        "[agents]\nallowed_model_providers=['openai']\n",
    )
    .unwrap();
    let mut fresh = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(harness.config.codex_home.to_path_buf())
        .cli_overrides(vec![
            ("model".into(), TomlValue::String("gpt-5.6-sol".into())),
            ("agents.max_depth".into(), TomlValue::Integer(8)),
        ])
        .build()
        .await
        .unwrap();
    for feature in [Feature::Sqlite, Feature::MultiAgentV2, Feature::ShellTool] {
        fresh.features.enable(feature).unwrap();
    }
    fresh
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    if environment_access {
        assert!(fresh.features.enabled(Feature::ShellTool));
        assert!(fresh.include_skill_instructions);
        assert!(fresh.bundled_skills_enabled());
        assert_eq!(
            fresh.permissions.shell_environment_policy.inherit,
            codex_protocol::config_types::ShellEnvironmentPolicyInherit::All
        );
    }
    assert!(!fresh.agent_roles.contains_key(role));
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        fresh.model_provider.clone(),
        fresh.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        harness.state_db.clone(),
    );
    let resumed = manager
        .start_thread(StartThreadOptions {
            initial_history: initial,
            session_source: Some(SessionSource::Exec),
            ..StartThreadOptions::new(fresh.clone())
        })
        .await
        .unwrap();
    assert!(
        resumed
            .thread
            .session
            .get_config()
            .await
            .features
            .enabled(Feature::ShellTool)
    );
    if environment_access {
        let turn = resumed
            .thread
            .session
            .new_turn_with_default_settings(
                "cold-current-environment-access".into(),
                Default::default(),
            )
            .await;
        let candidate = crate::agent::child_config::build_agent_resume_config(&turn).unwrap();
        assert_eq!(
            candidate.permissions.shell_environment_policy.inherit,
            codex_protocol::config_types::ShellEnvironmentPolicyInherit::All
        );
        let actual = resumed
            .thread
            .session
            .inherited_environment_snapshot()
            .await;
        assert_eq!(
            actual
                .primary()
                .unwrap()
                .config()
                .shell_environment_policy
                .inherit,
            codex_protocol::config_types::ShellEnvironmentPolicyInherit::All
        );
    }
    let error = resumed
        .thread
        .session
        .services
        .agent_control
        .send(SendRequest {
            caller: root.thread_id,
            target: AgentTarget::Id(target),
            resume_config: fresh,
            input: AgentInput::Message {
                message: AgentMessage::Plaintext("cold retry".into()),
                mode: MessageDeliveryMode::QueueOnly,
            },
            start_options: Default::default(),
        })
        .await
        .err()
        .expect("current builtin default must not erase old inherited restrictions");
    assert!(error.to_string().contains("sealed host ceiling"), "{error}");
    assert_eq!(manager.list_thread_ids().await, vec![root.thread_id]);
    assert_thread_not_loaded(&manager, target).await;
    if nested {
        assert_thread_not_loaded(&manager, worker.thread_id).await;
    }
}

#[test_case::test_case(false; "attachment command deny")]
#[test_case::test_case(true; "attachment MCP restriction")]
#[tokio::test]
async fn cold_attachment_restriction_declines_sealing_before_any_resume_effect(mcp: bool) {
    use codex_protocol::mcp_policy::EnvironmentMcpPolicy;
    use codex_protocol::mcp_policy::McpServerIdentity;
    use codex_protocol::mcp_policy::McpServerRequirement;
    let (home, _initial_config) = test_config_with_host_configuration(
        Vec::new(),
        Some("[agents]\nallowed_model_providers=['openai']\n"),
    )
    .await;
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .cli_overrides(vec![("agents.max_depth".into(), TomlValue::Integer(8))])
        .build()
        .await
        .unwrap();
    assert!(config.model_provider.auth.is_none());
    for feature in [Feature::Sqlite, Feature::MultiAgentV2, Feature::ShellTool] {
        config.features.enable(feature).unwrap();
    }
    config.model = Some("gpt-5.6-sol".into());
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    // Seal with the established API-key fixture. Install this helper only on the
    // real restored owner, then measure invocations around the denied child send.
    let marker = home.path().join("attachment-auth-invocations");
    let script = home.path().join(if cfg!(windows) {
        "attachment-auth.cmd"
    } else {
        "attachment-auth.sh"
    });
    std::fs::write(
        &script,
        if cfg!(windows) {
            "@echo off\r\necho invoked>>attachment-auth-invocations\r\necho synthetic\r\n"
        } else {
            "echo invoked >> attachment-auth-invocations\nprintf synthetic\n"
        },
    )
    .unwrap();
    let attachment_auth = serde_json::from_value(if cfg!(windows) {
        serde_json::json!({"command":"cmd.exe","args":["/D","/C",script],"cwd":home.path(),"timeout_ms":5000,"refresh_interval_ms":0})
    } else {
        serde_json::json!({"command":"sh","args":[script],"cwd":home.path(),"timeout_ms":5000,"refresh_interval_ms":0})
    }).unwrap();
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let root = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .unwrap();
    let control = root
        .thread
        .session
        .services
        .local_agent_runtime
        .control(root.thread.session.session_id());
    let child =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root.thread, "worker").await;
    let target = child.thread_id;
    let target_thread = harness.manager.get_thread(target).await.unwrap();
    target_thread
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
    let (before_config, before_settings, before_environments) = target_thread
        .session
        .native_agent_authority_snapshot()
        .await;
    let ready = before_environments.primary().unwrap();
    let original = ready.config().clone();
    assert!(original.exec_policy.is_none() && original.mcp_policy.is_none());
    let mut restricted = original.clone();
    if mcp {
        restricted.mcp_policy = Some(EnvironmentMcpPolicy {
            servers: Some(std::collections::BTreeMap::from([(
                "restricted-server".into(),
                McpServerRequirement::Identity {
                    identity: McpServerIdentity::Command {
                        command: "restricted-server".into(),
                    },
                },
            )])),
            plugins: None,
        });
    } else {
        let mut policy = codex_execpolicy::Policy::empty();
        policy
            .add_prefix_rule(
                &["restricted-command".into()],
                codex_execpolicy::Decision::Forbidden,
            )
            .unwrap();
        restricted.exec_policy = Some(codex_execpolicy::RequirementsExecPolicy::new(policy));
    }
    target_thread
        .environment_ready(&ready.selection(), restricted.clone())
        .await
        .expect("host owner must install the attachment restriction");
    let activated = target_thread
        .session
        .new_turn_with_default_settings("attachment-only-restriction".into(), Default::default())
        .await;
    drop(activated);
    let (after_config, after_settings, after_environments) = target_thread
        .session
        .native_agent_authority_snapshot()
        .await;
    assert_eq!(after_settings.cwd, before_settings.cwd);
    assert_eq!(
        after_config.permissions.effective_permission_profile(),
        before_config.permissions.effective_permission_profile()
    );
    assert_eq!(
        after_config.permissions.shell_environment_policy,
        before_config.permissions.shell_environment_policy
    );
    assert_eq!(after_config.workspace_roots, before_config.workspace_roots);
    assert_eq!(
        after_config.features.enabled(Feature::ShellTool),
        before_config.features.enabled(Feature::ShellTool)
    );
    assert_eq!(after_environments.primary().unwrap().config(), &restricted);
    let mut stripped = after_environments.primary().unwrap().config().clone();
    stripped.exec_policy = None;
    stripped.mcp_policy = None;
    assert_eq!(
        stripped, original,
        "only the actual attachment policy changed"
    );
    let global = target_thread.session.services.exec_policy.current();
    assert!(
        global.rules().is_empty()
            && global.network_rules().is_empty()
            && global.host_executables().is_empty()
    );
    assert!(
        target_thread
            .session
            .services
            .selected_capability_roots
            .is_empty()
    );
    persist_thread_for_tree_resume(&root.thread, "root attachment receipt fixture").await;
    persist_thread_for_tree_resume(&target_thread, "target attachment restriction fixture").await;
    wait_for_live_thread_spawn_children(&control, root.thread_id, &[target]).await;
    let target_rollout = target_thread
        .rollout_path()
        .expect("persisted target rollout");
    let (target_items, recorded_target, parse_errors) =
        codex_rollout::RolloutRecorder::load_rollout_items(&target_rollout)
            .await
            .expect("read actual flushed target rollout");
    assert_eq!(recorded_target, Some(target));
    assert_eq!(parse_errors, 0);
    assert!(
        history_contains_text(
            target_items.iter().filter_map(|item| match item {
                RolloutItem::ResponseItem(envelope) => Some(&envelope.item),
                _ => None,
            }),
            "target attachment restriction fixture"
        ),
        "actual target fixture message must be durable before sealing"
    );
    let stored_root = root
        .thread
        .read_thread(/*include_archived*/ true, /*include_history*/ true)
        .await
        .unwrap();
    let initial = InitialHistory::Resumed(ResumedHistory {
        history_revision: None,
        conversation_id: root.thread_id,
        history: Arc::new(stored_root.history.unwrap().items),
        rollout_path: stored_root.rollout_path,
    });
    let state = control.runtime.upgrade().unwrap();
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert!(report.timed_out.is_empty());
    assert!(
        state
            .read_native_authority_receipt(root.thread_id)
            .await
            .unwrap()
            .is_some(),
        "unrestricted root remains sealable"
    );
    assert!(
        state
            .read_native_authority_receipt(target)
            .await
            .unwrap()
            .is_none(),
        "opaque attachment restriction must produce NoProof, never a weaker sealed receipt"
    );
    let mut fresh = harness.config.clone();
    fresh.model_provider.auth = Some(attachment_auth);
    let state_db = harness.state_db.clone();
    let root_id = root.thread_id;
    drop(global);
    drop(target_thread);
    drop(root);
    drop(control);
    drop(state);
    let _home = harness._home;
    drop(harness.manager);
    drop(harness.control);
    drop(harness.config);
    drop(harness.state_db);
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        fresh.model_provider.clone(),
        fresh.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        state_db,
    );
    let resumed = manager
        .start_thread(StartThreadOptions {
            initial_history: initial,
            session_source: Some(SessionSource::Exec),
            ..StartThreadOptions::new(fresh.clone())
        })
        .await
        .unwrap();
    let runtime = &resumed.thread.session.services.local_agent_runtime;
    assert!(runtime.retained_authority(target).is_none());
    assert!(runtime.registry.evicted_environments(target).is_none());
    let restored = resumed
        .thread
        .session
        .inherited_environment_snapshot()
        .await;
    assert_eq!(
        restored.primary().unwrap().config(),
        &original,
        "owner removes only the attachment restriction"
    );
    let mut created = manager.subscribe_thread_created();
    let helper_before = std::fs::read(&marker).unwrap_or_default();
    let error = resumed
        .thread
        .session
        .services
        .agent_control
        .send(SendRequest {
            caller: root_id,
            target: AgentTarget::Id(target),
            resume_config: fresh,
            input: AgentInput::Message {
                message: AgentMessage::Plaintext("cold attachment retry".into()),
                mode: MessageDeliveryMode::QueueOnly,
            },
            start_options: Default::default(),
        })
        .await
        .err()
        .expect("attachment NoProof must deny cold resume before startup");
    assert!(
        error
            .to_string()
            .contains("cold effective authority is unsealed or unavailable"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&marker).unwrap_or_default(),
        helper_before,
        "denied send must invoke no auth helper"
    );
    assert!(
        matches!(
            created.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "denied send must publish no startup event"
    );
    assert_eq!(manager.list_thread_ids().await, vec![root_id]);
    assert_thread_not_loaded(&manager, target).await;
    assert!(runtime.retained_authority(target).is_none());
    assert!(runtime.registry.evicted_environments(target).is_none());
}

#[tokio::test]
async fn failed_saved_authority_invalidation_prevents_runtime_and_auth_helper_startup() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Sqlite).unwrap();
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let started = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .unwrap();
    let (id, thread) = (started.thread_id, started.thread);
    persist_thread_for_tree_resume(&thread, "sealed root before unavailable store").await;
    let stored = thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .unwrap();
    let initial = InitialHistory::Resumed(ResumedHistory {
        history_revision: None,
        conversation_id: id,
        history: Arc::new(stored.history.unwrap().items),
        rollout_path: stored.rollout_path,
    });
    let state = thread
        .session
        .services
        .local_agent_runtime
        .control(thread.session.session_id())
        .runtime
        .upgrade()
        .unwrap();
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert!(report.timed_out.is_empty());
    assert!(
        state
            .read_native_authority_receipt(id)
            .await
            .unwrap()
            .is_some()
    );
    let db = harness.state_db.as_ref().unwrap();
    db.close().await;
    let script = harness.config.codex_home.join(if cfg!(windows) {
        "blocked-reload.cmd"
    } else {
        "blocked-reload.sh"
    });
    std::fs::write(
        &script,
        if cfg!(windows) {
            "@echo off\r\necho invoked>blocked-reload-marker\r\necho synthetic\r\n"
        } else {
            "touch blocked-reload-marker\nprintf synthetic\n"
        },
    )
    .unwrap();
    let mut requested = harness.config.clone();
    let auth = if cfg!(windows) {
        serde_json::json!({"command":"cmd.exe","args":["/D","/C",script],"cwd":harness.config.codex_home,"timeout_ms":5000,"refresh_interval_ms":0})
    } else {
        serde_json::json!({"command":"sh","args":[script],"cwd":harness.config.codex_home,"timeout_ms":5000,"refresh_interval_ms":0})
    };
    requested.model_provider.auth = Some(serde_json::from_value(auth).unwrap());
    let error = harness
        .manager
        .start_thread(StartThreadOptions {
            initial_history: initial,
            session_source: Some(SessionSource::Exec),
            ..StartThreadOptions::new(requested)
        })
        .await
        .err()
        .expect("failed fence cannot execute or publish a reopened runtime");
    assert!(
        error
            .to_string()
            .contains("could not be invalidated before runtime startup"),
        "{error}"
    );
    assert!(
        !harness
            .config
            .codex_home
            .join("blocked-reload-marker")
            .exists()
    );
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

struct ColdResumeAdmissionRevoker {
    policy: codex_config::CloudConfigBundlePolicy,
    sharing: std::sync::atomic::AtomicBool,
    armed: std::sync::atomic::AtomicBool,
    revocations: std::sync::atomic::AtomicUsize,
}

impl ThreadInstructionsProvider for ColdResumeAdmissionRevoker {
    fn share_with_subagents(&self) -> bool {
        if !self.sharing.load(Ordering::SeqCst) {
            return false;
        }
        if self.armed.swap(false, Ordering::SeqCst) {
            let mut bundle = codex_config::CloudConfigBundle::default();
            bundle
                .config_toml
                .enterprise_managed
                .push(codex_config::CloudConfigFragment {
                    id: "cold-resume-late-revocation".into(),
                    name: "revoke before final admission".into(),
                    contents: "[agents]\nallowed_model_providers=[]".into(),
                });
            self.policy.observe_remote_bundle(&bundle).unwrap();
            self.revocations.fetch_add(1, Ordering::SeqCst);
        }
        true
    }

    fn load_thread_instructions(&self) -> LoadInstructionsFuture<'_> {
        Box::pin(async { LoadedUserInstructions::default() })
    }
}

#[tokio::test]
async fn denied_final_cold_admission_preserves_receipt_for_a_current_policy_retry() {
    let (home, mut config) = test_config_with_host_configuration(
        Vec::new(),
        Some("[agents]\nallowed_model_providers=['openai']\n"),
    )
    .await;
    for feature in [Feature::Sqlite, Feature::MultiAgentV2] {
        config.features.enable(feature).unwrap();
    }
    config.model = Some("gpt-5.6-sol".into());
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let root = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .unwrap();
    let source = thread_spawn_source(
        root.thread_id,
        &root.thread.session_source,
        /*depth*/ 1,
        /*agent_role*/ None,
        Some("cold_target".into()),
    )
    .unwrap();
    let child = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("cold target"),
            source,
            SpawnAgentOptions {
                parent_thread_id: Some(root.thread_id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let target = harness.manager.get_thread(child.thread_id).await.unwrap();
    persist_thread_for_tree_resume(&root.thread, "root for cold admission retry").await;
    persist_thread_for_tree_resume(&target, "target for cold admission retry").await;
    let turn = target.session.new_default_turn().await;
    assert!(
        target
            .session
            .persist_rollout_items(&[RolloutItem::TurnContext(turn.to_turn_context_item()),])
            .await
    );
    target.session.flush_rollout().await.unwrap();
    assert_eq!(
        target
            .read_thread(
                /*include_archived*/ false, /*include_history*/ false
            )
            .await
            .unwrap()
            .model
            .as_deref(),
        Some("gpt-5.6-sol")
    );
    let stored_root = root
        .thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .unwrap();
    let root_history = InitialHistory::Resumed(ResumedHistory {
        conversation_id: root.thread_id,
        history_revision: None,
        history: Arc::new(stored_root.history.unwrap().items),
        rollout_path: stored_root.rollout_path,
    });
    let original_state = harness.control.runtime.upgrade().unwrap();
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert!(report.timed_out.is_empty());
    let receipt = original_state
        .read_native_authority_receipt(child.thread_id)
        .await
        .unwrap()
        .expect("real stopped target must have a sealed host receipt");
    assert_eq!(receipt.epoch.parent_thread_id, Some(root.thread_id));
    assert_eq!(receipt.epoch.root_thread_id, root.thread_id);

    for revoke in [true, false] {
        let policy = codex_config::CloudConfigBundlePolicy::default();
        let mut snapshot = codex_config::CloudConfigBundleSnapshot {
            bundle: Ok(None),
            binding: None,
            config_server: None,
        };
        policy.publish_snapshot(&mut snapshot);
        let mut current = harness.config.clone();
        current.config_layer_stack = current
            .config_layer_stack
            .with_cloud_config_binding(snapshot.binding);
        let provider = Arc::new(ColdResumeAdmissionRevoker {
            policy,
            sharing: std::sync::atomic::AtomicBool::new(false),
            armed: std::sync::atomic::AtomicBool::new(false),
            revocations: std::sync::atomic::AtomicUsize::new(0),
        });
        let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
            CodexAuth::from_api_key("dummy"),
            current.model_provider.clone(),
            current.codex_home.to_path_buf(),
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            harness.state_db.clone(),
        );
        let resumed_root = manager
            .start_thread(StartThreadOptions {
                initial_history: root_history.clone(),
                session_source: Some(SessionSource::Exec),
                thread_instructions_provider: Some(provider.clone()),
                ..StartThreadOptions::new(current.clone())
            })
            .await
            .unwrap();
        let runtime = &resumed_root.thread.session.services.local_agent_runtime;
        assert!(
            runtime.retained_authority(child.thread_id).is_none(),
            "target must be genuinely cold"
        );
        provider.sharing.store(true, Ordering::SeqCst);
        provider.armed.store(revoke, Ordering::SeqCst);
        let state = runtime.upgrade().unwrap();
        assert_eq!(
            state
                .read_native_authority_receipt(child.thread_id)
                .await
                .unwrap(),
            Some(receipt.clone())
        );
        let result = runtime
            .control(resumed_root.thread.session.session_id())
            .resume_agent_from_rollout(current, child.thread_id, target.session_source.clone())
            .await;
        assert_eq!(
            provider.revocations.load(Ordering::SeqCst),
            usize::from(revoke),
            "real late manager callback must execute before interpreting the result"
        );
        if revoke {
            let error = match result {
                Ok(_) => panic!("late policy revocation must deny cold startup"),
                Err(error) => error,
            };
            assert!(
                error
                    .to_string()
                    .contains("revoked before startup admission"),
                "{error}"
            );
            assert!(runtime.retained_authority(child.thread_id).is_none());
            assert_eq!(manager.list_thread_ids().await, vec![root.thread_id]);
            assert_eq!(
                state
                    .read_native_authority_receipt(child.thread_id)
                    .await
                    .unwrap(),
                Some(receipt.clone()),
                "denied final admission cannot invalidate the only cold receipt"
            );
        } else {
            assert_eq!(
                result.unwrap(),
                child.thread_id,
                "a fresh current binding must permit the original cold retry"
            );
            let restored = manager.get_thread(child.thread_id).await.unwrap();
            assert_eq!(
                restored.thread_settings_snapshot().await.model,
                "gpt-5.6-sol"
            );
            assert_eq!(
                restored.session.session_id(),
                resumed_root.thread.session.session_id()
            );
        }
        let report = manager
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        assert!(report.timed_out.is_empty());
    }
}

// These fixtures keep the original prepared proof and cross the real lower
// manager boundary; no forged origin, map entry, or admission object is used.
async fn fresh_lifetime_permit(
    harness: &AgentControlHarness,
    parent: &Arc<CodexThread>,
    name: &str,
) -> (
    Config,
    SessionSource,
    crate::agent::child_provider_authorization::ChildLaunchPermit,
) {
    let parent_id = parent.session.thread_id;
    let mut source = thread_spawn_source(
        parent_id,
        &parent.session_source,
        next_thread_spawn_depth(&parent.session_source),
        /*agent_role*/ None,
        Some(name.into()),
    )
    .unwrap();
    let mut prepared = harness
        .control
        .prepare_spawn_fixture(&harness.config, &source, /*full_history*/ false)
        .await
        .expect("prepare on actual registered caller");
    if let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_role, .. }) = &mut source {
        *agent_role = prepared
            .prepared_child_authorization
            .as_ref()
            .unwrap()
            .role_name();
    }
    let permit = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            &harness.control.runtime,
            parent_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await
        .expect("consume proof while original caller is live");
    (prepared, source, permit)
}

async fn fresh_lifetime_lower_start(
    harness: &AgentControlHarness,
    parent: &Arc<CodexThread>,
    prepared: Config,
    source: SessionSource,
    permit: crate::agent::child_provider_authorization::ChildLaunchPermit,
) -> CodexResult<crate::thread_manager::NewThread> {
    harness
        .control
        .runtime
        .upgrade()
        .unwrap()
        .spawn_child_thread(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                session_source: Some(source),
                ..StartThreadOptions::new(prepared)
            },
            harness.control.clone(),
            Some(parent.session.thread_id),
            /*inherited_exec_policy*/ None,
            permit,
        )
        .await
}

#[test_case::test_case(false; "stopped after permit consumption")]
#[test_case::test_case(true; "same ID replaced after permit consumption")]
#[tokio::test]
async fn consumed_fresh_permit_rejects_stopped_or_replaced_caller(replace: bool) {
    let harness = AgentControlHarness::new().await;
    let started = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .unwrap();
    let parent = started.thread;
    let parent_id = started.thread_id;
    persist_thread_for_tree_resume(&parent, "post consume caller lifetime").await;
    let stored = parent
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .unwrap();
    let initial = InitialHistory::Resumed(ResumedHistory {
        history_revision: None,
        conversation_id: parent_id,
        history: Arc::new(stored.history.unwrap().items),
        rollout_path: stored.rollout_path,
    });
    let original = harness
        .control
        .runtime
        .retained_authority(parent_id)
        .unwrap();
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &parent, "post_consume_worker").await;
    parent.shutdown_and_wait().await.unwrap();
    assert!(!parent.is_running());
    let replacement = if replace {
        let removed = harness.manager.remove_thread(&parent_id).await.unwrap();
        assert!(Arc::ptr_eq(&removed, &parent));
        let replacement = harness
            .manager
            .start_thread_with_control_for_tests(
                StartThreadOptions {
                    initial_history: initial,
                    history_mode: Some(ThreadHistoryMode::Legacy),
                    session_source: Some(parent.session_source.clone()),
                    ..StartThreadOptions::new(harness.config.clone())
                },
                harness.control.clone(),
            )
            .await
            .unwrap();
        assert_eq!(replacement.thread_id, parent_id);
        assert!(!Arc::ptr_eq(&replacement.thread, &parent));
        assert_ne!(
            harness
                .control
                .runtime
                .retained_authority(parent_id)
                .unwrap()
                .epoch()
                .generation,
            original.epoch().generation
        );
        assert!(Arc::ptr_eq(
            &parent
                .session
                .services
                .local_agent_runtime
                .child_provider_origins,
            &replacement
                .thread
                .session
                .services
                .local_agent_runtime
                .child_provider_origins
        ));
        Some(replacement.thread)
    } else {
        None
    };
    let mut created = harness.manager.subscribe_thread_created();
    let outcome = fresh_lifetime_lower_start(&harness, &parent, prepared, source, permit).await;
    if let Ok(child) = &outcome {
        child.thread.shutdown_and_wait().await.unwrap();
        harness
            .manager
            .remove_thread(&child.thread_id)
            .await
            .unwrap();
    }
    let error = outcome
        .err()
        .expect("old consumed permit must not authorize lower startup");
    assert!(
        error.to_string().contains("child_provider_policy_conflict"),
        "{error}"
    );
    assert!(matches!(
        created.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(harness.manager.list_thread_ids().await, vec![parent_id]);
    if let Some(replacement) = replacement {
        let (prepared, source, permit) =
            fresh_lifetime_permit(&harness, &replacement, "current_replacement_worker").await;
        let child = fresh_lifetime_lower_start(&harness, &replacement, prepared, source, permit)
            .await
            .expect("new preparation on actual live replacement remains authorized");
        assert!(child.thread.is_running());
        child.thread.shutdown_and_wait().await.unwrap();
        replacement.shutdown_and_wait().await.unwrap();
    }
}

#[tokio::test]
async fn consumed_fresh_permit_accepts_current_live_caller_at_lower_startup() {
    let harness = AgentControlHarness::new().await;
    let (_, parent) = harness.start_thread().await;
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &parent, "current_live_worker").await;
    let child = fresh_lifetime_lower_start(&harness, &parent, prepared, source, permit)
        .await
        .unwrap();
    assert!(parent.is_running());
    assert!(child.thread.is_running());
    assert_eq!(
        child.thread.session.session_id(),
        parent.session.session_id()
    );
    assert_eq!(harness.manager.list_thread_ids().await.len(), 2);
    child.thread.shutdown_and_wait().await.unwrap();
    parent.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn fresh_caller_revocation_during_real_host_factory_await_blocks_final_admission() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let startup_control = Arc::new(std::sync::OnceLock::<LocalAgentControl>::new());
    let root_identity = Arc::new(std::sync::OnceLock::<codex_protocol::SessionId>::new());
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    )
    .with_agent_control_factory({
        let armed = Arc::clone(&armed);
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let startup_control = Arc::clone(&startup_control);
        let root_identity = Arc::clone(&root_identity);
        move |thread_id| {
            let control: Arc<dyn AgentControl> = Arc::new(
                startup_control
                    .get()
                    .unwrap()
                    .runtime
                    .control(*root_identity.get_or_init(|| thread_id.into())),
            );
            let pause = armed.swap(false, Ordering::SeqCst);
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            async move {
                if pause {
                    entered.notify_one();
                    release.notified().await;
                }
                Ok(control)
            }
        }
    });
    let control = manager.agent_control();
    assert!(startup_control.set(control.clone()).is_ok());
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let (_, parent) = harness.start_thread().await;
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &parent, "factory_paused_worker").await;
    armed.store(true, Ordering::SeqCst);
    let start = fresh_lifetime_lower_start(&harness, &parent, prepared, source, permit);
    tokio::pin!(start);
    tokio::select! {
        result = &mut start => { panic!("startup must reach real host factory before completion: {}", result.is_ok()); }
        reached = timeout(Duration::from_secs(10), entered.notified()) => {
            reached.expect("actual lower startup must reach the host factory await");
        }
    }
    assert!(
        !armed.load(Ordering::SeqCst),
        "factory must have consumed its real await barrier"
    );
    parent.shutdown_and_wait().await.unwrap();
    assert!(!parent.is_running());
    release.notify_one();
    let outcome = timeout(Duration::from_secs(10), start).await.unwrap();
    let error = outcome
        .err()
        .expect("revoked caller must fail final admission after factory await");
    assert!(
        error.to_string().contains("child_provider_policy_conflict"),
        "{error}"
    );
    assert_eq!(
        harness.manager.list_thread_ids().await,
        vec![parent.session.thread_id]
    );
}

async fn idle_fresh_capacity_resident(
    harness: &AgentControlHarness,
    root: &Arc<CodexThread>,
    name: &str,
) -> Arc<CodexThread> {
    let state = harness.control.runtime.upgrade().unwrap();
    let membership = harness.control.runtime.admit_start().unwrap();
    let slot = harness
        .control
        .reserve_v2_residency_slot(
            &state,
            &harness.config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .unwrap();
    let (prepared, source, permit) = fresh_lifetime_permit(harness, root, name).await;
    let child = fresh_lifetime_lower_start(harness, root, prepared, source, permit)
        .await
        .unwrap();
    slot.commit(child.thread_id);
    // This lower fixture has no task runner; publish an actual terminal status
    // without fabricating a manager registration or retained authority.
    let turn = child.thread.session.new_default_turn().await;
    child
        .thread
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                root_turn_id: None,
                turn_id: turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("idle capacity fixture".into()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    assert!(child.thread.session.active_turn.lock().await.is_none());
    child.thread
}

#[test_case::test_case(false; "sole idle caller remains resident on capacity error")]
#[test_case::test_case(true; "alternate idle resident is evicted instead of caller")]
#[tokio::test]
async fn fresh_host_idle_caller_is_protected_during_capacity_reservation(alternate: bool) {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.multi_agent_v2.max_concurrent_threads_per_session = if alternate { 3 } else { 2 };
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (_, root) = harness.start_thread().await;
    let caller = idle_fresh_capacity_resident(&harness, &root, "idle_authorizing_caller").await;
    let other = if alternate {
        Some(idle_fresh_capacity_resident(&harness, &root, "alternate_idle_resident").await)
    } else {
        None
    };
    assert!(caller.is_running());
    assert!(matches!(
        caller.agent_status().await,
        AgentStatus::Completed(_)
    ));
    let (mut prepared, source, _) =
        fresh_lifetime_permit(&harness, &caller, "capacity_worker").await;
    // Obtain a new real one-shot proof: the lower-boundary fixture consumed its
    // proof, whereas this case must cross the production fresh capacity path.
    prepared = harness
        .control
        .prepare_spawn_fixture(&prepared, &source, /*full_history*/ false)
        .await
        .unwrap();
    let outcome = harness
        .control
        .spawn(SpawnRequest {
            caller: caller.session.thread_id,
            config: prepared,
            input: AgentInput::UserInput(text_input("fresh capacity fixture")),
            source,
            options: SpawnAgentOptions {
                parent_thread_id: Some(caller.session.thread_id),
                ..Default::default()
            },
        })
        .await;
    let actual = harness
        .manager
        .get_thread(caller.session.thread_id)
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&actual, &caller));
    assert!(
        caller.is_running(),
        "fresh reservation must not stop its own authorizing caller"
    );
    if let Some(other) = other {
        let (agent, _) = outcome.expect("alternate idle resident makes room for fresh child");
        assert!(
            harness
                .manager
                .get_thread(other.session.thread_id)
                .await
                .is_err()
        );
        assert!(!other.is_running());
        let child = harness.manager.get_thread(agent.thread_id).await.unwrap();
        child.shutdown_and_wait().await.unwrap();
    } else {
        let error = match outcome {
            Ok(_) => panic!("sole protected caller leaves no capacity"),
            Err(error) => error,
        };
        assert!(
            matches!(error.details(), CodexErrorDetails::AgentLimitReached { .. }),
            "{error}"
        );
        assert_eq!(harness.manager.list_thread_ids().await.len(), 2);
    }
    caller.shutdown_and_wait().await.unwrap();
    root.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn consumed_fresh_permit_rejects_removed_but_still_running_caller() {
    let harness = AgentControlHarness::new().await;
    let (parent_id, parent) = harness.start_thread().await;
    let original = harness
        .control
        .runtime
        .retained_authority(parent_id)
        .unwrap();
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &parent, "removed_live_caller_worker").await;
    let removed = harness.manager.remove_thread(&parent_id).await.unwrap();
    assert!(Arc::ptr_eq(&removed, &parent));
    assert!(
        parent.is_running(),
        "manager removal alone does not stop held Session"
    );
    assert_eq!(
        harness
            .control
            .runtime
            .retained_authority(parent_id)
            .unwrap()
            .epoch()
            .generation,
        original.epoch().generation,
        "retained UUID alone cannot establish registration"
    );
    let outcome = fresh_lifetime_lower_start(&harness, &parent, prepared, source, permit).await;
    if let Ok(child) = &outcome {
        child.thread.shutdown_and_wait().await.unwrap();
        harness
            .manager
            .remove_thread(&child.thread_id)
            .await
            .unwrap();
    }
    parent.shutdown_and_wait().await.unwrap();
    let error = outcome
        .err()
        .expect("live removed caller must not admit its old permit");
    assert!(
        error.to_string().contains("child_provider_policy_conflict"),
        "{error}"
    );
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

#[test_case::test_case(0; "current caller reaches final cold admission")]
#[test_case::test_case(1; "caller stops during late cold startup await")]
#[test_case::test_case(2; "equal authority same ID caller replaced during late cold startup await")]
#[tokio::test]
async fn private_cold_resume_keeps_original_caller_through_final_factory_admission(stage: u8) {
    use crate::agent::child_provider_authorization::RetainedAgentAuthority;
    let (home, mut config) = test_config_with_host_configuration(
        Vec::new(),
        Some("[agents]\nallowed_model_providers=['openai']\n"),
    )
    .await;
    for feature in [Feature::Sqlite, Feature::MultiAgentV2] {
        config.features.enable(feature).unwrap();
    }
    config.model = Some("gpt-5.6-sol".into());
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let root = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        )
        .await
        .unwrap();
    let child = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("persist private caller lifetime target"),
            thread_spawn_source(
                root.thread_id,
                &root.thread.session_source,
                /*depth*/ 1,
                /*agent_role*/ None,
                Some("private_cold_lifetime_target".into()),
            )
            .unwrap(),
            SpawnAgentOptions {
                parent_thread_id: Some(root.thread_id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let target = harness.manager.get_thread(child.thread_id).await.unwrap();
    persist_thread_for_tree_resume(&root.thread, "cold lifetime root").await;
    persist_thread_for_tree_resume(&target, "cold lifetime target").await;
    let turn = target.session.new_default_turn().await;
    assert!(
        target
            .session
            .persist_rollout_items(&[RolloutItem::TurnContext(turn.to_turn_context_item())])
            .await
    );
    target.session.flush_rollout().await.unwrap();
    assert_eq!(
        target
            .read_thread(
                /*include_archived*/ false, /*include_history*/ false
            )
            .await
            .unwrap()
            .model
            .as_deref(),
        Some("gpt-5.6-sol")
    );
    let stored_root = root
        .thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .unwrap();
    let root_history = InitialHistory::Resumed(ResumedHistory {
        history_revision: None,
        conversation_id: root.thread_id,
        history: Arc::new(stored_root.history.unwrap().items),
        rollout_path: stored_root.rollout_path,
    });
    let original_state = harness.control.runtime.upgrade().unwrap();
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert!(report.timed_out.is_empty());
    let receipt = original_state
        .read_native_authority_receipt(child.thread_id)
        .await
        .unwrap()
        .expect("actual stopped target must have a sealed host receipt");
    assert_eq!(receipt.epoch.parent_thread_id, Some(root.thread_id));
    assert_eq!(receipt.epoch.root_thread_id, root.thread_id);

    // New manager/runtime: target has no retained ceiling, live Session or evicted
    // snapshot. The production host factory is the only pause introduced here.
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let startup_control = Arc::new(std::sync::OnceLock::<LocalAgentControl>::new());
    let root_identity = Arc::new(std::sync::OnceLock::<codex_protocol::SessionId>::new());
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        harness.config.model_provider.clone(),
        harness.config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        harness.state_db.clone(),
    )
    .with_agent_control_factory({
        let armed = Arc::clone(&armed);
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let startup_control = Arc::clone(&startup_control);
        let root_identity = Arc::clone(&root_identity);
        move |thread_id| {
            let control: Arc<dyn AgentControl> = Arc::new(
                startup_control
                    .get()
                    .unwrap()
                    .runtime
                    .control(*root_identity.get_or_init(|| thread_id.into())),
            );
            let pause = armed.swap(false, Ordering::SeqCst);
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            async move {
                if pause {
                    entered.notify_one();
                    release.notified().await;
                }
                Ok(control)
            }
        }
    });
    let control = manager.agent_control();
    assert!(startup_control.set(control.clone()).is_ok());
    let resumed_root = manager
        .start_thread_with_control_for_tests(
            StartThreadOptions {
                initial_history: root_history.clone(),
                history_mode: Some(ThreadHistoryMode::Legacy),
                session_source: Some(root.thread.session_source.clone()),
                ..StartThreadOptions::new(harness.config.clone())
            },
            control.clone(),
        )
        .await
        .unwrap();
    assert_eq!(resumed_root.thread_id, root.thread_id);
    let caller = resumed_root.thread;
    let runtime = &caller.session.services.local_agent_runtime;
    assert!(Arc::ptr_eq(
        &runtime.child_provider_origins,
        &control.runtime.child_provider_origins
    ));
    assert!(runtime.retained_authority(child.thread_id).is_none());
    assert!(
        runtime
            .registry
            .evicted_environments(child.thread_id)
            .is_none()
    );
    assert!(manager.get_thread(child.thread_id).await.is_err());
    let state = runtime.upgrade().unwrap();
    assert_eq!(
        state
            .read_native_authority_receipt(child.thread_id)
            .await
            .unwrap(),
        Some(receipt.clone())
    );
    let original_caller =
        RetainedAgentAuthority::capture(&caller.session, &caller.session_source).await;
    let original_origin = runtime
        .child_provider_origins
        .verified(caller.session.thread_id);
    let current = harness.config.clone();
    let resume_control = runtime.control(caller.session.session_id());
    armed.store(true, Ordering::SeqCst);
    let mut created = manager.subscribe_thread_created();
    let resume = resume_control.resume_agent_from_rollout(
        current,
        child.thread_id,
        target.session_source.clone(),
    );
    tokio::pin!(resume);
    tokio::select! {
        result = &mut resume => panic!("cold resume must reach actual factory await before completing: {}", result.is_ok()),
        reached = timeout(Duration::from_secs(10), entered.notified()) => {
            reached.expect("actual cold resume must reach late factory barrier");
        }
    }
    assert!(!armed.load(Ordering::SeqCst));
    assert!(runtime.retained_authority(child.thread_id).is_none());
    assert!(manager.get_thread(child.thread_id).await.is_err());
    assert_eq!(
        state
            .read_native_authority_receipt(child.thread_id)
            .await
            .unwrap(),
        Some(receipt.clone()),
        "pause must precede target startup fencing"
    );
    let replacement = if stage > 0 {
        caller.shutdown_and_wait().await.unwrap();
        assert!(!caller.is_running());
        assert!(Arc::ptr_eq(
            &manager.get_thread(caller.session.thread_id).await.unwrap(),
            &caller
        ));
        if stage == 2 {
            let removed = manager
                .remove_thread(&caller.session.thread_id)
                .await
                .unwrap();
            assert!(Arc::ptr_eq(&removed, &caller));
            let replacement = manager
                .start_thread_with_control_for_tests(
                    StartThreadOptions {
                        initial_history: root_history,
                        history_mode: Some(ThreadHistoryMode::Legacy),
                        session_source: Some(caller.session_source.clone()),
                        ..StartThreadOptions::new(harness.config.clone())
                    },
                    control.clone(),
                )
                .await
                .unwrap();
            assert_eq!(replacement.thread_id, caller.session.thread_id);
            assert!(replacement.thread.is_running());
            assert!(!Arc::ptr_eq(&replacement.thread, &caller));
            let current = RetainedAgentAuthority::capture(
                &replacement.thread.session,
                &replacement.thread.session_source,
            )
            .await;
            assert_ne!(
                current.epoch().generation,
                original_caller.epoch().generation
            );
            assert_eq!(current.config.as_ref(), original_caller.config.as_ref());
            assert_eq!(current.settings, original_caller.settings);
            assert!(current.scope == original_caller.scope);
            assert_eq!(
                runtime
                    .child_provider_origins
                    .verified(caller.session.thread_id),
                original_origin
            );
            assert!(Arc::ptr_eq(
                &runtime.child_provider_origins,
                &replacement
                    .thread
                    .session
                    .services
                    .local_agent_runtime
                    .child_provider_origins
            ));
            Some(replacement.thread)
        } else {
            None
        }
    } else {
        None
    };
    release.notify_one();
    let result = timeout(Duration::from_secs(15), resume).await.unwrap();
    let target_loaded = manager.get_thread(child.thread_id).await.ok();
    let receipt_after = state
        .read_native_authority_receipt(child.thread_id)
        .await
        .unwrap();
    let publication = created.try_recv();
    // Clean even a pre-fix unauthorized successful target before recording RED.
    if let Some(target) = target_loaded.as_ref() {
        target.shutdown_and_wait().await.unwrap();
        manager.remove_thread(&child.thread_id).await.unwrap();
    }
    if let Some(replacement) = replacement {
        replacement.shutdown_and_wait().await.unwrap();
    }
    if stage == 0 {
        assert_eq!(
            result.unwrap(),
            child.thread_id,
            "unchanged real cold caller must still resume"
        );
        assert!(target_loaded.is_some());
        caller.shutdown_and_wait().await.unwrap();
    } else {
        let error = match result {
            Ok(_) => panic!(
                "late stale private caller unexpectedly authorized actual cold target startup"
            ),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("child_provider_policy_conflict"),
            "{error}"
        );
        assert!(
            target_loaded.is_none(),
            "stale private caller cannot publish target Session"
        );
        assert!(matches!(
            publication,
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(
            receipt_after,
            Some(receipt),
            "denied final admission preserves cold target receipt"
        );
    }
}

#[test_case::test_case(0; "completed eviction then explicit unloaded close")]
#[test_case::test_case(1; "explicit close blocks in flight eviction completion")]
#[test_case::test_case(2; "stale evicted allocation cannot retire real replacement")]
#[tokio::test]
async fn real_private_resume_caller_lifecycle_distinguishes_eviction_and_revocation(stage: u8) {
    use crate::agent::child_provider_authorization::RetainedAgentAuthority;
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::Sqlite).unwrap();
    config.model = Some("gpt-5.6-sol".into());
    config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (_, root) = harness.start_paginated_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let child =
        spawn_v2_reload_test_child(&control, harness.config.clone(), &root, "lifecycle_caller")
            .await;
    let caller = harness.manager.get_thread(child.thread_id).await.unwrap();
    persist_thread_for_tree_resume(&caller, "retained caller lifecycle").await;
    caller
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
    caller.session.mark_interrupted();
    *caller.session.active_turn.lock().await = None;
    let _ = caller.session.input_queue.drain_mailbox_input_items().await;
    assert!(caller.is_running());
    let runtime = &caller.session.services.local_agent_runtime;
    let epoch = RetainedAgentAuthority::capture(&caller.session, &caller.session_source).await;
    assert!(
        runtime
            .with_resume_caller(&epoch, /*live*/ true, || Ok(()))
            .is_ok()
    );
    // Busy is not successful residency removal and cannot manufacture a stopped
    // caller exception. The original caller still runs with the same real epoch.
    let busy_barrier = hold_synchronous_read_barrier(Arc::clone(&caller.residency_gate)).await;
    assert_eq!(
        harness
            .manager
            .try_evict_v2_thread(Arc::clone(&caller))
            .await
            .unwrap(),
        ThreadEvictionOutcome::Busy
    );
    assert!(
        runtime
            .with_resume_caller(&epoch, /*live*/ false, || Ok(()))
            .is_err()
    );
    busy_barrier.release();

    if stage == 1 {
        let state = runtime.upgrade().unwrap();
        // A real read guard on a synchronous thread allows eviction's initial
        // lookup and shutdown, but blocks exact map removal/completion. The
        // async test owns only its release/join handle, never the lock guard.
        let map_barrier = hold_synchronous_read_barrier(Arc::clone(&state.threads)).await;
        let eviction = harness.manager.try_evict_v2_thread(Arc::clone(&caller));
        tokio::pin!(eviction);
        tokio::select! {
            result = &mut eviction => panic!("map read barrier must retain in-flight eviction: {result:?}"),
            stopped = timeout(Duration::from_secs(10), caller.wait_until_terminated()) => {
                stopped.expect("actual eviction must stop the original caller");
            }
        }
        assert!(!caller.is_running());
        let close = control.close_agent(child.thread_id);
        tokio::pin!(close);
        poll_fn(|cx| match close.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("actual close must await map removal behind read barrier"),
        })
        .await;
        map_barrier.release();
        let (evicted, closed) = tokio::join!(eviction, close);
        assert!(matches!(
            evicted.unwrap(),
            ThreadEvictionOutcome::Evicted | ThreadEvictionOutcome::NotCurrent
        ));
        closed.expect("actual loaded close should finish after barrier release");
        assert!(
            runtime
                .with_resume_caller(&epoch, /*live*/ false, || Ok(()))
                .is_err(),
            "close tombstone must survive in-flight eviction completion"
        );
        assert!(
            runtime
                .registry
                .evicted_environments(child.thread_id)
                .is_none()
        );
    } else {
        assert_eq!(
            harness
                .manager
                .try_evict_v2_thread(Arc::clone(&caller))
                .await
                .unwrap(),
            ThreadEvictionOutcome::Evicted
        );
        assert!(!caller.is_running());
        assert!(harness.manager.get_thread(child.thread_id).await.is_err());
        assert!(
            runtime
                .retained_authority(child.thread_id)
                .unwrap()
                .same_registration(&epoch)
        );
        assert!(
            runtime
                .with_resume_caller(&epoch, /*live*/ false, || Ok(()))
                .is_ok(),
            "only actual completed same-epoch host eviction grants stopped-caller exception"
        );
        assert!(
            runtime
                .registry
                .evicted_environments(child.thread_id)
                .is_some()
        );
        if stage == 0 {
            assert_matches!(
                control.close_agent(child.thread_id).await.unwrap(),
                AgentInfo::Unloaded(_)
            );
            assert!(
                runtime
                    .with_resume_caller(&epoch, /*live*/ false, || Ok(()))
                    .is_err(),
                "explicit unloaded close must revoke completed eviction"
            );
            let snapshot =
                RetainedAgentAuthority::capture(&caller.session, &caller.session_source).await;
            assert!(snapshot.same_registration(&epoch));
            runtime.retire_authority(snapshot);
            assert!(
                runtime
                    .with_resume_caller(&epoch, /*live*/ false, || Ok(()))
                    .is_err(),
                "ordinary same-epoch snapshot retention must not reset a revocation tombstone"
            );
            assert_eq!(
                harness
                    .manager
                    .try_evict_v2_thread(Arc::clone(&caller))
                    .await
                    .unwrap(),
                ThreadEvictionOutcome::NotCurrent
            );
            assert!(
                runtime
                    .with_resume_caller(&epoch, /*live*/ false, || Ok(()))
                    .is_err()
            );
        } else {
            // The real native reload supplies a new Session/UUID under the same
            // thread ID. Old snapshot data cannot authenticate the new allocation.
            let loaded = control
                .resume_agent_from_rollout(
                    harness.config.clone(),
                    child.thread_id,
                    caller.session_source.clone(),
                )
                .await
                .expect("root can reload actual evicted native caller");
            assert_eq!(loaded, child.thread_id);
            let replacement = harness.manager.get_thread(child.thread_id).await.unwrap();
            assert!(!Arc::ptr_eq(&replacement, &caller));
            let replacement_epoch =
                RetainedAgentAuthority::capture(&replacement.session, &replacement.session_source)
                    .await;
            assert_ne!(
                replacement_epoch.epoch().generation,
                epoch.epoch().generation
            );
            assert!(
                runtime
                    .with_resume_caller(&epoch, /*live*/ false, || Ok(()))
                    .is_err()
            );
            assert!(
                runtime
                    .with_resume_caller(&replacement_epoch, /*live*/ true, || Ok(()))
                    .is_ok()
            );
            // Delayed removal and eviction of the held original must leave the
            // replacement's Current epoch and actual registration untouched.
            assert!(
                harness
                    .manager
                    .remove_thread_if_matches(&child.thread_id, &caller)
                    .await
                    .is_none()
            );
            assert_eq!(
                harness
                    .manager
                    .try_evict_v2_thread(Arc::clone(&caller))
                    .await
                    .unwrap(),
                ThreadEvictionOutcome::NotCurrent
            );
            assert!(Arc::ptr_eq(
                &harness.manager.get_thread(child.thread_id).await.unwrap(),
                &replacement
            ));
            assert!(
                runtime
                    .with_resume_caller(&replacement_epoch, /*live*/ true, || Ok(()))
                    .is_ok()
            );
            replacement.shutdown_and_wait().await.unwrap();
        }
    }
    root.shutdown_and_wait().await.unwrap();
}

// The actual guard lives solely on the synchronous OS thread. This handle owns
// only channels/join cleanup; it cannot conceal a guard across an async await.
struct SynchronousReadBarrier {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SynchronousReadBarrier {
    fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        self.thread
            .take()
            .unwrap()
            .join()
            .expect("synchronous read barrier thread must finish cleanly");
    }
}

impl Drop for SynchronousReadBarrier {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            // On assertion failure/cancellation, release before joining. A
            // secondary thread panic must not double-panic during unwinding.
            let _ = thread.join();
        }
    }
}

async fn hold_synchronous_read_barrier<T: Send + Sync + 'static>(
    lock: Arc<tokio::sync::RwLock<T>>,
) -> SynchronousReadBarrier {
    let (ready, acquired) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        // Bounded nonblocking acquisition also makes cancellation cleanup safe
        // on a current-thread runtime: joining cannot wait for that runtime to
        // run a task that releases a competing lock.
        let deadline = std::time::Instant::now() + Duration::from_secs(/*secs*/ 10);
        loop {
            match released.try_recv() {
                Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            if let Ok(guard) = lock.try_read() {
                let _ = ready.send(());
                assert!(
                    released.recv_timeout(Duration::from_secs(/*secs*/ 30)).is_ok(),
                    "synchronous read barrier must be explicitly released"
                );
                drop(guard);
                return;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(/*millis*/ 1));
        }
    });
    let barrier = SynchronousReadBarrier {
        release: Some(release),
        thread: Some(thread),
    };
    acquired
        .await
        .expect("synchronous thread must acquire the real read barrier");
    barrier
}

struct PendingFreshRevocationPersistence {
    thread_id: ThreadId,
    context: codex_thread_store::PersistContext,
    release: tokio::sync::oneshot::Sender<()>,
}

struct FreshRevocationPersistenceGate {
    inner: InMemoryThreadStore,
    armed: std::sync::Mutex<Option<ThreadId>>,
    entered: tokio::sync::mpsc::UnboundedSender<PendingFreshRevocationPersistence>,
}

macro_rules! delegate_fresh_revocation_store {
    ($(fn $name:ident($param:ident: $params:ty) -> $result:ty;)*) => {
        $(fn $name(&self, $param: $params) -> codex_thread_store::ThreadStoreFuture<'_, $result> {
            ThreadStore::$name(&self.inner, $param)
        })*
    };
}

impl ThreadStore for FreshRevocationPersistenceGate {
    fn begin_native_authority_epoch_owned(
        &self,
        epoch: codex_thread_store::NativeAuthorityEpoch,
        owner: codex_thread_store::ThreadWriterOwner,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.begin_native_authority_epoch_owned(epoch, owner)
    }

    fn create_thread_owned(
        &self,
        params: codex_thread_store::CreateThreadParams,
    ) -> codex_thread_store::ThreadStoreFuture<'_, codex_thread_store::ThreadWriterOwner> {
        self.inner.create_thread_owned(params)
    }
    fn resume_thread_owned(
        &self,
        params: codex_thread_store::ResumeThreadParams,
        preparation: Option<codex_thread_store::ThreadStoreResumePreparation>,
    ) -> codex_thread_store::ThreadStoreFuture<
        '_,
        (Arc<Vec<RolloutItem>>, codex_thread_store::ThreadWriterOwner),
    > {
        self.inner.resume_thread_owned(params, preparation)
    }
    fn finish_thread_owned(
        &self,
        thread: ThreadId,
        owner: codex_thread_store::ThreadWriterOwner,
        discard: bool,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.finish_thread_owned(thread, owner, discard)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    delegate_fresh_revocation_store! {
        fn create_thread(params: codex_thread_store::CreateThreadParams) -> ();
        fn resume_thread(params: codex_thread_store::ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
        fn append_items(params: codex_thread_store::AppendThreadItemsParams) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredThreadHistory;
        fn load_latest_model_context(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredModelContext;
        fn read_thread(params: codex_thread_store::ReadThreadParams) -> codex_thread_store::StoredThread;
        fn read_thread_by_rollout_path(params: codex_thread_store::ReadThreadByRolloutPathParams) -> codex_thread_store::StoredThread;
        fn list_threads(params: codex_thread_store::ListThreadsParams) -> codex_thread_store::ThreadPage;
        fn update_thread_metadata(params: codex_thread_store::UpdateThreadMetadataParams) -> Option<codex_thread_store::StoredThread>;
        fn archive_thread(params: codex_thread_store::ArchiveThreadParams) -> ();
        fn unarchive_thread(params: codex_thread_store::ArchiveThreadParams) -> codex_thread_store::StoredThread;
        fn delete_thread(params: codex_thread_store::DeleteThreadParams) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
    }

    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: codex_thread_store::PersistContext,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let pause = {
                let mut armed = self.armed.lock().unwrap();
                if *armed == Some(thread_id) {
                    armed.take();
                    true
                } else {
                    false
                }
            };
            if pause {
                let (release, released) = tokio::sync::oneshot::channel();
                self.entered
                    .send(PendingFreshRevocationPersistence {
                        thread_id,
                        context,
                        release,
                    })
                    .expect("actual persistence gate receiver");
                released
                    .await
                    .expect("release actual caller persistence before shutdown submission");
            }
            self.inner.persist_thread(thread_id, context).await
        })
    }
}

#[test_case::test_case(0; "current fresh caller remains authorized")]
#[test_case::test_case(1; "actual explicit close revokes still running fresh caller")]
#[test_case::test_case(2; "actual tree shutdown cannot miss fresh child after revocation")]
#[tokio::test]
async fn fresh_revoked_running_caller_cannot_cross_final_admission(stage: u8) {
    use crate::agent::child_provider_authorization::RetainedAgentAuthority;
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.model = Some("gpt-5.6-sol".into());
    let (entered_store, mut pending_store) = tokio::sync::mpsc::unbounded_channel();
    let store = Arc::new(FreshRevocationPersistenceGate {
        inner: InMemoryThreadStore::default(),
        armed: std::sync::Mutex::new(None),
        entered: entered_store,
    });
    let factory_armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let factory_entered = Arc::new(tokio::sync::Notify::new());
    let factory_release = Arc::new(tokio::sync::Notify::new());
    let startup_control = Arc::new(std::sync::OnceLock::<LocalAgentControl>::new());
    let root_identity = Arc::new(std::sync::OnceLock::<codex_protocol::SessionId>::new());
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    )
    .with_agent_control_factory({
        let factory_armed = Arc::clone(&factory_armed);
        let factory_entered = Arc::clone(&factory_entered);
        let factory_release = Arc::clone(&factory_release);
        let startup_control = Arc::clone(&startup_control);
        let root_identity = Arc::clone(&root_identity);
        move |thread_id| {
            let control: Arc<dyn AgentControl> = Arc::new(
                startup_control
                    .get()
                    .unwrap()
                    .runtime
                    .control(*root_identity.get_or_init(|| thread_id.into())),
            );
            let pause = factory_armed.swap(false, Ordering::SeqCst);
            let entered = Arc::clone(&factory_entered);
            let release = Arc::clone(&factory_release);
            async move {
                if pause {
                    entered.notify_one();
                    release.notified().await;
                }
                Ok(control)
            }
        }
    });
    let control = manager.agent_control();
    assert!(startup_control.set(control.clone()).is_ok());
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let (caller_id, caller) = harness.start_thread().await;
    assert_eq!(
        root_identity.get().copied(),
        Some(caller.session.session_id())
    );
    caller.ensure_rollout_materialized().await;
    caller.session.flush_rollout().await.unwrap();
    let epoch = RetainedAgentAuthority::capture(&caller.session, &caller.session_source).await;
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &caller, "revocation_fenced_worker").await;
    factory_armed.store(true, Ordering::SeqCst);
    let child_start = fresh_lifetime_lower_start(&harness, &caller, prepared, source, permit);
    tokio::pin!(child_start);
    tokio::select! {
        result = &mut child_start => panic!("actual fresh start must reach late factory barrier: {}", result.is_ok()),
        entered = timeout(Duration::from_secs(/*secs*/ 10), factory_entered.notified()) => {
            entered.expect("actual fresh start reaches factory await");
        }
    }
    assert!(!factory_armed.load(Ordering::SeqCst));
    assert_eq!(harness.manager.list_thread_ids().await, vec![caller_id]);
    if stage == 0 {
        factory_release.notify_one();
        let child = timeout(Duration::from_secs(/*secs*/ 10), child_start)
            .await
            .unwrap()
            .unwrap();
        assert!(caller.is_running());
        assert!(child.thread.is_running());
        child.thread.shutdown_and_wait().await.unwrap();
        harness
            .manager
            .remove_thread(&child.thread_id)
            .await
            .unwrap();
        caller.shutdown_and_wait().await.unwrap();
        return;
    }

    *store.armed.lock().unwrap() = Some(caller_id);
    // Both real routes reach shutdown_live_agent's caller persistence AFTER
    // committed registry revocation and BEFORE submitting Op::Shutdown. For the
    // tree route its descendant snapshot also precedes this awaited gate.
    let shutdown = async {
        if stage == 1 {
            harness.control.close_agent(caller_id).await.map(|_| ())
        } else {
            harness
                .control
                .shutdown_agent_tree(caller_id)
                .await
                .map(|_| ())
        }
    };
    tokio::pin!(shutdown);
    let blocked = tokio::select! {
        result = &mut shutdown => panic!("actual close/shutdown must block before Op::Shutdown: {result:?}"),
        entered = timeout(Duration::from_secs(/*secs*/ 10), pending_store.recv()) => {
            entered.expect("actual caller persistence reaches gate").expect("persistence gate remains live")
        }
    };
    assert_eq!(blocked.thread_id, caller_id);
    assert_eq!(
        blocked.context,
        codex_thread_store::PersistContext::Standard
    );
    assert!(
        caller.is_running(),
        "revocation must precede actual Session shutdown"
    );
    assert!(
        *caller.session.fresh_child_admission_live.lock().unwrap(),
        "separate fresh signal must still be live"
    );
    assert!(
        harness
            .control
            .runtime
            .retained_authority(caller_id)
            .unwrap()
            .same_registration(&epoch)
    );
    assert!(
        harness
            .control
            .runtime
            .with_resume_caller(&epoch, /*live*/ true, || Ok(()))
            .is_err(),
        "actual close must already have committed its registry tombstone"
    );
    let revoked_preparation = harness
        .control
        .prepare_spawn_fixture(
            &harness.config,
            &thread_spawn_source(
                caller_id,
                &caller.session_source,
                next_thread_spawn_depth(&caller.session_source),
                /*agent_role*/ None,
                Some("revoked_capture_worker".into()),
            )
            .unwrap(),
            /*full_history*/ false,
        )
        .await;
    factory_release.notify_one();
    let outcome = timeout(Duration::from_secs(/*secs*/ 10), child_start)
        .await
        .unwrap();
    let started_id = outcome.as_ref().ok().map(|child| child.thread_id);
    assert!(
        caller.is_running(),
        "caller remains blocked before shutdown submission during final decision"
    );
    blocked
        .release
        .send(())
        .expect("release actual shutdown persistence");
    timeout(Duration::from_secs(/*secs*/ 10), shutdown)
        .await
        .unwrap()
        .unwrap();
    assert!(!caller.is_running());
    let surviving_child = match started_id {
        Some(child_id) => harness.manager.get_thread(child_id).await.ok(),
        None => None,
    };
    let survivor_present = surviving_child.is_some();
    tracing::info!(
        "FRESH_REVOCATION_OBSERVED stage={stage} started={started_id:?} survivor_after_shutdown={survivor_present}"
    );
    if let Some(child) = surviving_child {
        child.shutdown_and_wait().await.unwrap();
        harness
            .manager
            .remove_thread(&child.session.thread_id)
            .await
            .unwrap();
    }
    let error = match outcome {
        Ok(_) => panic!(
            "explicitly revoked still-running caller unexpectedly authorized actual fresh startup"
        ),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("child_provider_policy_conflict"),
        "{error}"
    );
    assert!(
        !survivor_present,
        "completed tree shutdown must leave no child admitted after its descendant snapshot"
    );
    assert!(
        revoked_preparation.is_err(),
        "fresh capture must reject same-epoch explicit revocation before preparation I/O"
    );
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

struct PendingPostAdmissionCreate {
    child: ThreadId,
    parent: ThreadId,
    release: tokio::sync::oneshot::Sender<()>,
}

struct PostAdmissionCreateGate {
    inner: InMemoryThreadStore,
    armed_parent: std::sync::Mutex<Option<ThreadId>>,
    armed_resume: std::sync::Mutex<Option<(ThreadId, ThreadId)>>,
    entered: tokio::sync::mpsc::UnboundedSender<PendingPostAdmissionCreate>,
}

impl ThreadStore for PostAdmissionCreateGate {
    fn begin_native_authority_epoch_owned(
        &self,
        epoch: codex_thread_store::NativeAuthorityEpoch,
        owner: codex_thread_store::ThreadWriterOwner,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.begin_native_authority_epoch_owned(epoch, owner)
    }

    fn create_thread_owned(
        &self,
        params: codex_thread_store::CreateThreadParams,
    ) -> codex_thread_store::ThreadStoreFuture<'_, codex_thread_store::ThreadWriterOwner> {
        Box::pin(async move {
            let armed = *self.armed_parent.lock().unwrap();
            if params.parent_thread_id.is_some() && params.parent_thread_id == armed {
                let parent = armed.expect("armed actual parent");
                assert_eq!(params.source.parent_thread_id(), Some(parent));
                assert!(matches!(
                    &params.source,
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                ));
                *self.armed_parent.lock().unwrap() = None;
                let (release, released) = tokio::sync::oneshot::channel();
                self.entered
                    .send(PendingPostAdmissionCreate {
                        child: params.thread_id,
                        parent,
                        release,
                    })
                    .expect("actual store gate receiver");
                released
                    .await
                    .expect("release actual post-admission create");
            }
            self.inner.create_thread_owned(params).await
        })
    }
    fn resume_thread_owned(
        &self,
        params: codex_thread_store::ResumeThreadParams,
        preparation: Option<codex_thread_store::ThreadStoreResumePreparation>,
    ) -> codex_thread_store::ThreadStoreFuture<
        '_,
        (Arc<Vec<RolloutItem>>, codex_thread_store::ThreadWriterOwner),
    > {
        Box::pin(async move {
            let armed = *self.armed_resume.lock().unwrap();
            if let Some((target, parent)) = armed.filter(|(target, _)| *target == params.thread_id)
            {
                *self.armed_resume.lock().unwrap() = None;
                let history = params
                    .history
                    .as_ref()
                    .expect("actual host-read resume history");
                assert!(history.iter().any(|item| matches!(item, RolloutItem::SessionMeta(line) if line.meta.id == target && line.meta.parent_thread_id == Some(parent))));
                let (release, released) = tokio::sync::oneshot::channel();
                self.entered
                    .send(PendingPostAdmissionCreate {
                        child: target,
                        parent,
                        release,
                    })
                    .unwrap();
                released
                    .await
                    .expect("release actual post-admission resume writer");
            }
            self.inner.resume_thread_owned(params, preparation).await
        })
    }
    fn finish_thread_owned(
        &self,
        thread: ThreadId,
        owner: codex_thread_store::ThreadWriterOwner,
        discard: bool,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.finish_thread_owned(thread, owner, discard)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    delegate_fresh_revocation_store! {
        fn resume_thread(params: codex_thread_store::ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
        fn append_items(params: codex_thread_store::AppendThreadItemsParams) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredThreadHistory;
        fn load_latest_model_context(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredModelContext;
        fn read_thread(params: codex_thread_store::ReadThreadParams) -> codex_thread_store::StoredThread;
        fn read_thread_by_rollout_path(params: codex_thread_store::ReadThreadByRolloutPathParams) -> codex_thread_store::StoredThread;
        fn list_threads(params: codex_thread_store::ListThreadsParams) -> codex_thread_store::ThreadPage;
        fn update_thread_metadata(params: codex_thread_store::UpdateThreadMetadataParams) -> Option<codex_thread_store::StoredThread>;
        fn archive_thread(params: codex_thread_store::ArchiveThreadParams) -> ();
        fn unarchive_thread(params: codex_thread_store::ArchiveThreadParams) -> codex_thread_store::StoredThread;
        fn delete_thread(params: codex_thread_store::DeleteThreadParams) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
    }

    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: codex_thread_store::PersistContext,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread_id, context)
    }

    fn create_thread(
        &self,
        params: codex_thread_store::CreateThreadParams,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let armed = *self.armed_parent.lock().unwrap();
            if params.parent_thread_id.is_some() && params.parent_thread_id == armed {
                let parent = armed.expect("armed actual parent");
                assert_eq!(params.source.parent_thread_id(), Some(parent));
                assert!(matches!(
                    &params.source,
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                ));
                *self.armed_parent.lock().unwrap() = None;
                let (release, released) = tokio::sync::oneshot::channel();
                self.entered
                    .send(PendingPostAdmissionCreate {
                        child: params.thread_id,
                        parent,
                        release,
                    })
                    .expect("actual store gate receiver");
                released
                    .await
                    .expect("release actual post-admission create");
            }
            self.inner.create_thread(params).await
        })
    }
}

#[test_case::test_case(0; "current post admission startup registers")]
#[test_case::test_case(1; "explicit close fences admitted unregistered child")]
#[test_case::test_case(2; "tree shutdown fences admitted unregistered child")]
#[tokio::test]
async fn post_admission_startup_cannot_escape_completed_subtree_shutdown(stage: u8) {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.model = Some("gpt-5.6-sol".into());
    let (entered, mut pending) = tokio::sync::mpsc::unbounded_channel();
    let store = Arc::new(PostAdmissionCreateGate {
        inner: InMemoryThreadStore::default(),
        armed_parent: std::sync::Mutex::new(None),
        armed_resume: std::sync::Mutex::new(None),
        entered,
    });
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let (caller_id, caller) = harness.start_thread().await;
    caller.ensure_rollout_materialized().await;
    caller.session.flush_rollout().await.unwrap();
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &caller, "post_admission_child").await;
    *store.armed_parent.lock().unwrap() = Some(caller_id);
    let start = fresh_lifetime_lower_start(&harness, &caller, prepared, source, permit);
    tokio::pin!(start);
    let blocked = tokio::select! {
        result = &mut start => panic!("must reach real post-admission store create: {}", result.is_ok()),
        reached = timeout(Duration::from_secs(/*secs*/ 10), pending.recv()) => reached.unwrap().expect("actual child store create"),
    };
    assert_eq!(blocked.parent, caller_id);
    assert_ne!(blocked.child, caller_id);
    assert!(caller.is_running());
    assert_eq!(harness.manager.list_thread_ids().await, vec![caller_id]);
    if stage == 0 {
        blocked.release.send(()).unwrap();
        let child = timeout(Duration::from_secs(/*secs*/ 10), start)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.thread_id, blocked.child);
        assert!(child.thread.is_running());
        assert!(Arc::ptr_eq(
            &child.thread,
            &harness.manager.get_thread(blocked.child).await.unwrap()
        ));
        timeout(
            Duration::from_secs(/*secs*/ 10),
            child.thread.shutdown_and_wait(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            harness
                .manager
                .remove_thread(&blocked.child)
                .await
                .is_some()
        );
        timeout(Duration::from_secs(/*secs*/ 10), caller.shutdown_and_wait())
            .await
            .unwrap()
            .unwrap();
        assert!(harness.manager.remove_thread(&caller_id).await.is_some());
        return;
    }
    let shutdown = async {
        if stage == 1 {
            harness.control.close_agent(caller_id).await.map(|_| ())
        } else {
            harness
                .control
                .shutdown_agent_tree(caller_id)
                .await
                .map(|_| ())
        }
    };
    tokio::pin!(shutdown);
    // Keep polling the independent startup as teardown runs: a repaired owner
    // may cancel and drain it instead of only rejecting its final registration.
    // The store gate itself remains closed until teardown has completed.
    let early = timeout(Duration::from_secs(/*secs*/ 10), async {
        tokio::select! {
            result = &mut shutdown => {
                result.unwrap();
                None
            },
            outcome = &mut start => {
                assert!(outcome.is_err(), "held real store creation cannot publish a successful child");
                shutdown.await.unwrap();
                Some(outcome)
            },
        }
    }).await.expect("actual subtree teardown completes while the admitted store gate is held");
    assert!(!caller.is_running());
    assert!(harness.manager.get_thread(caller_id).await.is_err());
    assert!(harness.manager.get_thread(blocked.child).await.is_err());
    let released = blocked.release.send(());
    let outcome = if let Some(outcome) = early {
        outcome
    } else {
        released.expect("independent unfinished startup still owns its store gate");
        timeout(Duration::from_secs(/*secs*/ 10), start)
            .await
            .unwrap()
    };
    let registered = harness.manager.get_thread(blocked.child).await.ok();
    let survivor_present = registered.is_some();
    let survivor =
        registered.or_else(|| outcome.as_ref().ok().map(|child| Arc::clone(&child.thread)));
    let survivor_running = survivor.as_ref().is_some_and(|child| child.is_running());
    tracing::info!(
        "POST_ADMISSION_SUBTREE_OBSERVED stage={stage} returned_success={} survivor_present={survivor_present} survivor_running={survivor_running}",
        outcome.is_ok()
    );
    if let Some(child) = survivor {
        timeout(Duration::from_secs(/*secs*/ 10), child.shutdown_and_wait())
            .await
            .unwrap()
            .unwrap();
        let _ = harness.manager.remove_thread(&blocked.child).await;
    }
    assert!(
        outcome.is_err(),
        "completed subtree teardown must reject independently awaited in-flight native startup: returned_success={} survivor_present={survivor_present} survivor_running={survivor_running}",
        outcome.is_ok()
    );
    assert!(
        !survivor_present && !survivor_running,
        "no admitted child may register or run after completed subtree teardown: survivor_present={survivor_present} survivor_running={survivor_running}"
    );
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

fn loaded_fixture_policy_denial(error: &str) -> bool {
    [
        "child_provider_not_authorized",
        "child_provider_untrusted_source",
        "child_provider_policy_conflict",
    ]
    .iter()
    .any(|code| error.contains(code))
}

async fn loaded_custom_provider_resident(
    harness: &AgentControlHarness,
    parent: &Arc<CodexThread>,
) -> Arc<CodexThread> {
    let runtime = &harness.control.runtime;
    let state = runtime.upgrade().unwrap();
    let membership = runtime.admit_start().unwrap();
    let slot = harness
        .control
        .reserve_v2_residency_slot(
            &state,
            &harness.config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .unwrap();
    let turn = parent.session.new_default_turn().await;
    let step = StepContext::for_test(turn);
    let mut prepared = crate::agent::child_config::prepare_agent_spawn_config(
        &parent.session,
        &step,
        crate::agent::child_config::SpawnConfigOptions {
            resolved_provider: None,
            model_provider: Some("a"),
            version: crate::agent::child_config::SpawnConfigVersion::V2,
            full_history_fork: false,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("real trusted custom-provider preparation")
    .config;
    assert_eq!(prepared.model_provider_id, "a");
    let mut reservation = runtime.registry.reserve_spawn_slot(None).unwrap();
    let (source, mut metadata) = harness
        .control
        .prepare_thread_spawn(
            &mut reservation,
            &prepared,
            parent.session.thread_id,
            next_thread_spawn_depth(&parent.session_source),
            Some(AgentPath::from_string("/root/loaded_custom_authority_target".into()).unwrap()),
            None,
            None,
        )
        .unwrap();
    let permit = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            runtime,
            parent.session.thread_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await
        .expect("consume actual prepared custom-provider proof");
    let child = fresh_lifetime_lower_start(harness, parent, prepared, source, permit)
        .await
        .unwrap();
    metadata.agent_id = Some(child.thread_id);
    reservation.commit(metadata);
    slot.commit(child.thread_id);
    // An idle lower-start has not activated its speculative rollout yet.
    // Materialize through the actual Session before the owner route reads it.
    child.thread.ensure_rollout_materialized().await;
    child.thread.flush_rollout().await.unwrap();
    assert_eq!(
        child.thread.multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    let stored = child.thread.read_thread(true, true).await.unwrap();
    let history = stored
        .history
        .expect("actual lower-start persisted startup history");
    assert!(history.items.iter().any(|item| matches!(item,
        RolloutItem::SessionMeta(line) if line.meta.id == child.thread_id && line.meta.multi_agent_version == Some(MultiAgentVersion::V2)
    )), "actual persisted V2 metadata required by owner-loaded path");
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(child.thread_id)
            .is_some()
    );
    assert!(child.thread.session.active_turn.lock().await.is_none());
    assert_eq!(child.thread.config_snapshot().await.model_provider_id, "a");
    child.thread
}

// These regressions deliberately retain already loaded allocations. Reloading a
// target would exercise a different, already guarded authorization path.
#[test_case::test_case(0, 0; "loaded user input current")]
#[test_case::test_case(1, 0; "loaded trigger current")]
#[test_case::test_case(2, 0; "loaded owner ensure current")]
#[test_case::test_case(0, 1; "loaded user input host revoked")]
#[test_case::test_case(1, 1; "loaded trigger host revoked")]
#[test_case::test_case(2, 1; "loaded owner ensure host revoked")]
#[test_case::test_case(0, 2; "loaded user input managed revoked")]
#[test_case::test_case(1, 2; "loaded trigger managed revoked")]
#[test_case::test_case(2, 2; "loaded owner ensure managed revoked")]
#[test_case::test_case(0, 3; "loaded user input executor scope widened")]
#[test_case::test_case(1, 3; "loaded trigger executor scope widened")]
#[test_case::test_case(2, 3; "loaded owner ensure executor scope widened")]
#[tokio::test]
async fn loaded_native_authority_rechecked_before_delivery(route: u8, change: u8) {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    let target = loaded_custom_provider_resident(&harness, &parent).await;
    let target_id = target.session.thread_id;
    assert!(target.is_running());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));
    let before_ops = harness.manager.captured_ops().len();

    match change {
        0 => {}
        1 => {
            let mut value: TomlValue =
                toml::from_str(&std::fs::read_to_string(&host_file).unwrap()).unwrap();
            let before_definition = value["model_providers"]["a"].clone();
            let before_defaults = value["agents"]["provider_defaults"]["a"].clone();
            value["agents"]["allowed_model_providers"] =
                TomlValue::Array(vec![TomlValue::String("openai".into())]);
            std::fs::write(&host_file, toml::to_string(&value).unwrap()).unwrap();
            assert_eq!(value["model_providers"]["a"], before_definition);
            assert_eq!(value["agents"]["provider_defaults"]["a"], before_defaults);
            let next = ConfigBuilder::without_managed_config_for_tests()
                .codex_home(harness.config.codex_home.to_path_buf())
                .harness_overrides(crate::config::ConfigOverrides {
                    cwd: Some(project.path().to_path_buf()),
                    ..Default::default()
                })
                .build()
                .await
                .unwrap();
            let current = parent.config().await;
            assert_eq!(
                parent.refresh_runtime_config(current, next).await,
                crate::ConfigRefreshOutcome::Published
            );
            let published = parent.config().await;
            let effective = published.config_layer_stack.effective_config();
            assert!(
                !effective["agents"]["allowed_model_providers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|id| id.as_str() == Some("a"))
            );
            assert_eq!(published.model_provider_id, "openai");
        }
        2 => {
            let mut bundle = codex_config::CloudConfigBundle::default();
            bundle
                .config_toml
                .enterprise_managed
                .push(codex_config::CloudConfigFragment {
                    id: "loaded-native-revocation".into(),
                    name: "revoke loaded provider".into(),
                    contents: "[agents]\nallowed_model_providers=['openai']\n".into(),
                });
            policy.observe_remote_bundle(&bundle).unwrap();
            assert!(
                parent
                    .config()
                    .await
                    .config_layer_stack
                    .cloud_config_binding()
                    .is_some_and(|binding| binding.read().status
                        != codex_config::CloudConfigBundleBindingStatus::Current)
            );
        }
        3 => {
            let environments = parent.session.inherited_environment_snapshot().await;
            let ready = environments.primary().expect("actual invoking executor");
            let mut owner = ready.config().clone();
            owner.permission_profile = codex_protocol::models::PermissionProfileSnapshot::legacy(
                PermissionProfile::Disabled,
            );
            let mut selection = ready.selection();
            selection.config = EnvironmentConfigState::Ready(owner);
            parent
                .update_thread_settings(ThreadSettingsOverrides {
                    permission_profile: Some(PermissionProfile::Disabled),
                    environments: Some(TurnEnvironmentRequests::new(
                        parent.thread_settings_snapshot().await.cwd,
                        vec![selection.into_request()],
                    )),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        _ => unreachable!(),
    }
    let caller_turn = parent
        .session
        .new_turn_with_default_settings("loaded-authority-followup".into(), Default::default())
        .await;
    let resume_config =
        crate::agent::child_config::build_agent_resume_config(&caller_turn).unwrap();
    let captured = crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
        &parent.session,
        &parent.session_source,
    )
    .await;
    assert!(
        captured.scope
            == crate::agent::child_provider_authorization::ChildProviderScope::from_step(
                &resume_config,
                &caller_turn.initial_environments,
            ),
        "caller settings and actual executor scope must agree before the loaded operation"
    );
    if change == 3 {
        let ready = caller_turn
            .initial_environments
            .primary()
            .expect("actual widened caller executor");
        let filesystem = ready
            .permission_profile_with_workspace_roots()
            .file_system_sandbox_policy();
        assert!(
            filesystem.can_write_local_path_with_cwd(&host_file, caller_turn.config.cwd.as_path())
        );
        assert!(
            filesystem
                .can_write_local_path_with_cwd(&catalog_file, caller_turn.config.cwd.as_path())
        );
    }
    let outcome = if route == 2 {
        harness
            .control
            .ensure_child_loaded(parent_id, target_id)
            .await
    } else {
        harness
            .control
            .send(SendRequest {
                caller: parent_id,
                target: AgentTarget::Id(target_id),
                resume_config,
                input: if route == 0 {
                    AgentInput::UserInput(text_input("loaded authority followup"))
                } else {
                    AgentInput::Message {
                        message: AgentMessage::Plaintext("loaded authority followup".into()),
                        mode: MessageDeliveryMode::TriggerTurn,
                    }
                },
                start_options: Default::default(),
            })
            .await
            .map(|_| ())
    };
    if change == 0 && route == 0 {
        wait_for_recorded_user_message(target.as_ref(), "loaded authority followup").await;
    }
    let after_ops = harness.manager.captured_ops().len();
    let same_allocation = Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap(),
    );
    // Always finish the owned actual Sessions before an expected RED assertion.
    tokio::time::timeout(Duration::from_secs(10), target.shutdown_and_wait())
        .await
        .expect("bounded target cleanup")
        .unwrap();
    harness.manager.remove_thread(&target_id).await;
    tokio::time::timeout(Duration::from_secs(10), parent.shutdown_and_wait())
        .await
        .expect("bounded parent cleanup")
        .unwrap();
    harness.manager.remove_thread(&parent_id).await;
    assert!(
        same_allocation,
        "the test must never reconstruct or replace the target"
    );
    if change == 0 {
        outcome.expect("current policy must allow loaded native work");
        // The existing ops observer captures inter-agent delivery, not UserTurn.
        // User input is proved by the actual recorded message above.
        assert_eq!(after_ops - before_ops, usize::from(route == 1));
    } else {
        let error = outcome
            .err()
            .expect("revoked loaded authority must reject before accepting new work");
        assert!(loaded_fixture_policy_denial(&error.to_string()), "{error}");
        assert_eq!(
            after_ops, before_ops,
            "revocation must precede actual target submission"
        );
    }
}

async fn post_admission_lifecycle_harness(
    version: MultiAgentVersion,
) -> (
    AgentControlHarness,
    Arc<PostAdmissionCreateGate>,
    tokio::sync::mpsc::UnboundedReceiver<PendingPostAdmissionCreate>,
) {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    if version == MultiAgentVersion::V1 {
        config.features.disable(Feature::MultiAgentV2);
    }
    config.model = Some("gpt-5.6-sol".into());
    config.multi_agent_v2.max_concurrent_threads_per_session = 8;
    config.agent_max_depth = 8;
    let (entered, pending) = tokio::sync::mpsc::unbounded_channel();
    let store = Arc::new(PostAdmissionCreateGate {
        inner: InMemoryThreadStore::default(),
        armed_parent: std::sync::Mutex::new(None),
        armed_resume: std::sync::Mutex::new(None),
        entered,
    });
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    (harness, store, pending)
}

#[test_case::test_case(false, false; "loaded owner teardown wins late grandchild")]
#[test_case::test_case(false, true; "loaded owner registration wins and joins teardown")]
#[test_case::test_case(true, false; "unloaded owner teardown fences loaded grandchild caller")]
#[test_case::test_case(true, true; "unloaded owner includes registered great grandchild")]
#[tokio::test]
async fn post_admission_descendant_registration_orders_with_ancestor_shutdown(
    unloaded: bool,
    registration_wins: bool,
) {
    let (harness, store, mut pending) =
        post_admission_lifecycle_harness(MultiAgentVersion::V2).await;
    let (root_id, root) = harness.start_thread().await;
    let sibling = idle_fresh_capacity_resident(&harness, &root, "independent_sibling").await;
    let owner = idle_fresh_capacity_resident(&harness, &root, "subtree_owner").await;
    let caller = idle_fresh_capacity_resident(&harness, &owner, "loaded_grandchild_caller").await;
    let owner_id = owner.session.thread_id;
    let caller_id = caller.session.thread_id;
    if unloaded {
        assert_eq!(
            harness
                .manager
                .try_evict_v2_thread(Arc::clone(&owner))
                .await
                .unwrap(),
            ThreadEvictionOutcome::Evicted
        );
        assert!(!owner.is_running());
        assert!(harness.manager.get_thread(owner_id).await.is_err());
        assert!(caller.is_running());
    }
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &caller, "late_descendant").await;
    *store.armed_parent.lock().unwrap() = Some(caller_id);
    let start = fresh_lifetime_lower_start(&harness, &caller, prepared, source, permit);
    tokio::pin!(start);
    let blocked = tokio::select! {
        result = &mut start => panic!("must enter actual post-admission descendant writer create: {}", result.is_ok()),
        entered = timeout(Duration::from_secs(10), pending.recv()) => entered.unwrap().unwrap(),
    };
    assert_eq!(blocked.parent, caller_id);
    let mut release = Some(blocked.release);
    let early_child = if registration_wins {
        release.take().unwrap().send(()).unwrap();
        Some(
            timeout(Duration::from_secs(10), &mut start)
                .await
                .unwrap()
                .unwrap()
                .thread,
        )
    } else {
        None
    };
    let teardown = timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(owner_id),
    )
    .await
    .unwrap();
    if unloaded {
        assert!(matches!(
            teardown.unwrap_err().details(),
            CodexErrorDetails::ThreadNotFound(_)
        ));
    } else {
        teardown.unwrap();
    }
    assert!(!caller.is_running());
    assert!(harness.manager.get_thread(caller_id).await.is_err());
    if let Some(child) = early_child {
        assert!(
            !child.is_running(),
            "registration winning the shared map ordering must be included in teardown"
        );
    } else {
        release.take().unwrap().send(()).unwrap();
        assert!(
            timeout(Duration::from_secs(10), &mut start)
                .await
                .unwrap()
                .is_err(),
            "teardown winning the shared map ordering must reject late registration"
        );
    }
    assert!(harness.manager.get_thread(blocked.child).await.is_err());
    if !registration_wins {
        assert!(
            matches!(store.inner.load_history(codex_thread_store::LoadThreadHistoryParams { thread_id: blocked.child, include_archived: true }).await,
            Err(codex_thread_store::ThreadStoreError::ThreadNotFound { thread_id }) if thread_id == blocked.child),
            "denied fresh acquisition must leave no provisional history"
        );
    }
    assert!(
        root.is_running(),
        "subtree closure must not stop the outside root"
    );
    assert!(
        sibling.is_running(),
        "subtree closure must not stop a sibling"
    );
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

async fn post_admission_registered_idle_child(
    harness: &AgentControlHarness,
    parent: &Arc<CodexThread>,
    name: &str,
) -> Arc<CodexThread> {
    let parent_id = parent.session.thread_id;
    let source = thread_spawn_source(
        parent_id,
        &parent.session_source,
        next_thread_spawn_depth(&parent.session_source),
        None,
        Some(name.into()),
    )
    .unwrap();
    let mut prepared = harness
        .control
        .prepare_spawn_fixture(&harness.config, &source, false)
        .await
        .unwrap();
    let mut reservation = harness
        .control
        .runtime
        .registry
        .reserve_spawn_slot(None)
        .unwrap();
    let role = prepared
        .prepared_child_authorization
        .as_ref()
        .unwrap()
        .role_name();
    let (source, mut metadata) = harness
        .control
        .prepare_thread_spawn(
            &mut reservation,
            &prepared,
            parent_id,
            next_thread_spawn_depth(&parent.session_source),
            source.get_agent_path(),
            role,
            None,
        )
        .unwrap();
    let permit = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            &harness.control.runtime,
            parent_id,
            &prepared,
            false,
            &source,
        )
        .await
        .unwrap();
    let state = harness.control.runtime.upgrade().unwrap();
    let membership = harness.control.runtime.admit_start().unwrap();
    let slot = harness
        .control
        .reserve_v2_residency_slot(&state, &prepared, &membership, Some(parent_id))
        .await
        .unwrap();
    let child = fresh_lifetime_lower_start(harness, parent, prepared, source, permit)
        .await
        .unwrap();
    metadata.agent_id = Some(child.thread_id);
    reservation.commit(metadata);
    slot.commit(child.thread_id);
    child.thread.ensure_rollout_materialized().await;
    child.thread.session.flush_rollout().await.unwrap();
    assert!(child.thread.session.active_turn.lock().await.is_none());
    child.thread
}

#[test_case::test_case(0; "outside caller current owned resume positive")]
#[test_case::test_case(1; "outside caller cannot resume target closed after admission")]
#[test_case::test_case(2; "second explicit close invalidates new resume of already closed target")]
#[tokio::test]
async fn post_admission_resume_target_revision_fences_outside_live_caller(stage: u8) {
    let (harness, store, mut pending) =
        post_admission_lifecycle_harness(MultiAgentVersion::V2).await;
    let (root_id, root) = harness.start_thread().await;
    let target = post_admission_registered_idle_child(&harness, &root, "resume_target").await;
    let target_id = target.session.thread_id;
    let source = target.session_source.clone();
    if stage == 2 {
        harness.control.close_agent(target_id).await.unwrap();
    } else {
        assert_eq!(
            harness
                .manager
                .try_evict_v2_thread(Arc::clone(&target))
                .await
                .unwrap(),
            ThreadEvictionOutcome::Evicted
        );
    }
    assert!(!target.is_running());
    assert!(harness.manager.get_thread(target_id).await.is_err());
    *store.armed_resume.lock().unwrap() = Some((target_id, root_id));
    let resume =
        harness
            .control
            .resume_agent_from_rollout(harness.config.clone(), target_id, source);
    tokio::pin!(resume);
    let blocked = tokio::select! {
        result = &mut resume => panic!("must enter actual resume acquisition after admission: {}", result.is_ok()),
        entered = timeout(Duration::from_secs(10), pending.recv()) => entered.unwrap().unwrap(),
    };
    assert_eq!(blocked.child, target_id);
    assert_eq!(blocked.parent, root_id);
    if stage != 0 {
        let closed = harness.control.close_agent(target_id).await;
        if stage == 1 {
            closed.unwrap();
        } else {
            assert!(matches!(
                closed.unwrap_err().details(),
                CodexErrorDetails::ThreadNotFound(_)
            ));
        }
    }
    assert!(
        root.is_running(),
        "actual caller outside the target subtree stays current"
    );
    blocked.release.send(()).unwrap();
    let result = timeout(Duration::from_secs(10), resume).await.unwrap();
    if stage == 0 {
        assert_eq!(result.unwrap(), target_id);
        assert!(
            harness
                .manager
                .get_thread(target_id)
                .await
                .unwrap()
                .is_running()
        );
    } else {
        assert!(
            result
                .as_ref()
                .err()
                .is_some_and(|error| error.to_string().contains("child_provider_policy_conflict")),
            "target teardown revision must invalidate an admitted resume even when its actual outside caller remains live"
        );
        assert!(harness.manager.get_thread(target_id).await.is_err());
        assert!(
            !store
                .inner
                .load_history(codex_thread_store::LoadThreadHistoryParams {
                    thread_id: target_id,
                    include_archived: true
                })
                .await
                .unwrap()
                .items
                .is_empty(),
            "denied resume must retain existing history"
        );
        // A deliberate new operation after the completed close captures the new
        // revision and remains supported; the previous failed operation grants nothing.
        let reopened = timeout(
            Duration::from_secs(10),
            harness.control.resume_agent_from_rollout(
                harness.config.clone(),
                target_id,
                target.session_source.clone(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reopened, target_id);
    }
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

#[test_case::test_case(0; "v1 current post admission startup registers")]
#[test_case::test_case(1; "v1 explicit close fences admitted unregistered child")]
#[test_case::test_case(2; "v1 tree shutdown fences admitted unregistered child")]
#[tokio::test]
async fn post_admission_v1_startup_cannot_escape_completed_subtree_shutdown(stage: u8) {
    let (harness, store, mut pending) =
        post_admission_lifecycle_harness(MultiAgentVersion::V1).await;
    let (caller_id, caller) = harness.start_thread().await;
    caller.ensure_rollout_materialized().await;
    caller.session.flush_rollout().await.unwrap();
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &caller, "post_admission_child").await;
    assert_eq!(caller.multi_agent_version(), Some(MultiAgentVersion::V1));
    *store.armed_parent.lock().unwrap() = Some(caller_id);
    let start = fresh_lifetime_lower_start(&harness, &caller, prepared, source, permit);
    tokio::pin!(start);
    let blocked = tokio::select! {
        result = &mut start => panic!("must reach real post-admission store create: {}", result.is_ok()),
        reached = timeout(Duration::from_secs(/*secs*/ 10), pending.recv()) => reached.unwrap().expect("actual child store create"),
    };
    assert_eq!(blocked.parent, caller_id);
    assert_ne!(blocked.child, caller_id);
    assert!(caller.is_running());
    assert_eq!(harness.manager.list_thread_ids().await, vec![caller_id]);
    if stage == 0 {
        blocked.release.send(()).unwrap();
        let child = timeout(Duration::from_secs(/*secs*/ 10), start)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.thread_id, blocked.child);
        assert!(child.thread.is_running());
        assert!(Arc::ptr_eq(
            &child.thread,
            &harness.manager.get_thread(blocked.child).await.unwrap()
        ));
        timeout(
            Duration::from_secs(/*secs*/ 10),
            child.thread.shutdown_and_wait(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            harness
                .manager
                .remove_thread(&blocked.child)
                .await
                .is_some()
        );
        timeout(Duration::from_secs(/*secs*/ 10), caller.shutdown_and_wait())
            .await
            .unwrap()
            .unwrap();
        assert!(harness.manager.remove_thread(&caller_id).await.is_some());
        return;
    }
    let shutdown = async {
        if stage == 1 {
            harness.control.close_agent(caller_id).await.map(|_| ())
        } else {
            harness
                .control
                .shutdown_agent_tree(caller_id)
                .await
                .map(|_| ())
        }
    };
    tokio::pin!(shutdown);
    // Keep polling the independent startup as teardown runs: a repaired owner
    // may cancel and drain it instead of only rejecting its final registration.
    // The store gate itself remains closed until teardown has completed.
    let early = timeout(Duration::from_secs(/*secs*/ 10), async {
        tokio::select! {
            result = &mut shutdown => {
                result.unwrap();
                None
            },
            outcome = &mut start => {
                assert!(outcome.is_err(), "held real store creation cannot publish a successful child");
                shutdown.await.unwrap();
                Some(outcome)
            },
        }
    }).await.expect("actual subtree teardown completes while the admitted store gate is held");
    assert!(!caller.is_running());
    assert!(harness.manager.get_thread(caller_id).await.is_err());
    assert!(harness.manager.get_thread(blocked.child).await.is_err());
    let released = blocked.release.send(());
    let outcome = if let Some(outcome) = early {
        outcome
    } else {
        released.expect("independent unfinished startup still owns its store gate");
        timeout(Duration::from_secs(/*secs*/ 10), start)
            .await
            .unwrap()
    };
    let registered = harness.manager.get_thread(blocked.child).await.ok();
    let survivor_present = registered.is_some();
    let survivor =
        registered.or_else(|| outcome.as_ref().ok().map(|child| Arc::clone(&child.thread)));
    let survivor_running = survivor.as_ref().is_some_and(|child| child.is_running());
    tracing::info!(
        "POST_ADMISSION_SUBTREE_OBSERVED stage={stage} returned_success={} survivor_present={survivor_present} survivor_running={survivor_running}",
        outcome.is_ok()
    );
    if let Some(child) = survivor {
        timeout(Duration::from_secs(/*secs*/ 10), child.shutdown_and_wait())
            .await
            .unwrap()
            .unwrap();
        let _ = harness.manager.remove_thread(&blocked.child).await;
    }
    assert!(
        outcome.is_err(),
        "completed subtree teardown must reject independently awaited in-flight native startup: returned_success={} survivor_present={survivor_present} survivor_running={survivor_running}",
        outcome.is_ok()
    );
    assert!(
        !survivor_present && !survivor_running,
        "no admitted child may register or run after completed subtree teardown: survivor_present={survivor_present} survivor_running={survivor_running}"
    );
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

#[tokio::test]
async fn post_admission_stale_caller_cannot_register_or_retire_same_id_replacement() {
    let (harness, store, mut pending) =
        post_admission_lifecycle_harness(MultiAgentVersion::V2).await;
    let (root_id, root) = harness.start_thread().await;
    root.ensure_rollout_materialized().await;
    root.session.flush_rollout().await.unwrap();
    let state = harness.control.runtime.upgrade().unwrap();
    let stored = state
        .read_stored_thread(codex_thread_store::ReadThreadParams {
            thread_id: root_id,
            include_archived: true,
            include_history: true,
        })
        .await
        .unwrap();
    let initial = InitialHistory::Resumed(ResumedHistory {
        history_revision: None,
        conversation_id: root_id,
        history: Arc::new(stored.history.unwrap().items),
        rollout_path: stored.rollout_path,
    });
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &root, "replacement_race_child").await;
    *store.armed_parent.lock().unwrap() = Some(root_id);
    let start = fresh_lifetime_lower_start(&harness, &root, prepared, source, permit);
    tokio::pin!(start);
    let blocked = tokio::select! {
        result = &mut start => panic!("must reach admitted real writer acquisition: {}", result.is_ok()),
        entered = timeout(Duration::from_secs(10), pending.recv()) => entered.unwrap().unwrap(),
    };
    timeout(Duration::from_secs(10), root.shutdown_and_wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        harness
            .manager
            .remove_thread_if_matches(&root_id, &root)
            .await
            .is_some()
    );
    let replacement = timeout(
        Duration::from_secs(10),
        harness.manager.start_thread_with_control_for_tests(
            StartThreadOptions {
                initial_history: initial,
                history_mode: Some(ThreadHistoryMode::Legacy),
                session_source: Some(root.session_source.clone()),
                ..StartThreadOptions::new(harness.config.clone())
            },
            harness.control.clone(),
        ),
    )
    .await
    .unwrap()
    .unwrap()
    .thread;
    assert_eq!(replacement.session.thread_id, root_id);
    assert!(!Arc::ptr_eq(&root, &replacement));
    let epoch = harness.control.runtime.retained_authority(root_id).unwrap();
    assert!(replacement.is_running());
    blocked.release.send(()).unwrap();
    assert!(
        timeout(Duration::from_secs(10), start)
            .await
            .unwrap()
            .is_err()
    );
    assert!(harness.manager.get_thread(blocked.child).await.is_err());
    assert!(
        matches!(store.inner.load_history(codex_thread_store::LoadThreadHistoryParams { thread_id: blocked.child, include_archived: true }).await,
        Err(codex_thread_store::ThreadStoreError::ThreadNotFound { thread_id }) if thread_id == blocked.child),
        "stale native startup must clean only its own fresh history"
    );
    assert!(Arc::ptr_eq(
        &harness.manager.get_thread(root_id).await.unwrap(),
        &replacement
    ));
    assert!(replacement.is_running());
    assert!(
        harness
            .control
            .runtime
            .retained_authority(root_id)
            .unwrap()
            .same_registration(&epoch)
    );
    assert!(
        harness
            .manager
            .remove_thread_if_matches(&root_id, &root)
            .await
            .is_none()
    );
    assert!(replacement.is_running());
    timeout(
        Duration::from_secs(10),
        harness.control.shutdown_agent_tree(root_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

struct LegacyRootOnlyThreadStore {
    inner: InMemoryThreadStore,
}
impl ThreadStore for LegacyRootOnlyThreadStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    delegate_fresh_revocation_store! {
        fn create_thread(params: codex_thread_store::CreateThreadParams) -> ();
        fn resume_thread(params: codex_thread_store::ResumeThreadParams) -> Arc<Vec<RolloutItem>>;
        fn append_items(params: codex_thread_store::AppendThreadItemsParams) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: codex_thread_store::LoadThreadHistoryParams) -> codex_thread_store::StoredThreadHistory;
        fn read_thread(params: codex_thread_store::ReadThreadParams) -> codex_thread_store::StoredThread;
        fn read_thread_by_rollout_path(params: codex_thread_store::ReadThreadByRolloutPathParams) -> codex_thread_store::StoredThread;
        fn list_threads(params: codex_thread_store::ListThreadsParams) -> codex_thread_store::ThreadPage;
        fn update_thread_metadata(params: codex_thread_store::UpdateThreadMetadataParams) -> Option<codex_thread_store::StoredThread>;
        fn archive_thread(params: codex_thread_store::ArchiveThreadParams) -> ();
        fn unarchive_thread(params: codex_thread_store::ArchiveThreadParams) -> codex_thread_store::StoredThread;
        fn delete_thread(params: codex_thread_store::DeleteThreadParams) -> ();
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
    }
    fn persist_thread(
        &self,
        id: ThreadId,
        context: codex_thread_store::PersistContext,
    ) -> codex_thread_store::ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(id, context)
    }
}

#[tokio::test]
async fn legacy_custom_store_root_remains_supported_without_native_owned_writer_fallback() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.model = Some("gpt-5.6-sol".into());
    let store = Arc::new(LegacyRootOnlyThreadStore {
        inner: InMemoryThreadStore::default(),
    });
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let (root_id, root) = harness.start_thread().await;
    root.ensure_rollout_materialized().await;
    root.session.flush_rollout().await.unwrap();
    assert!(root.is_running());
    assert_eq!(store.inner.calls().await.create_thread, 1);
    let (prepared, source, permit) =
        fresh_lifetime_permit(&harness, &root, "unsupported_native_owner").await;
    let denied = timeout(
        Duration::from_secs(10),
        fresh_lifetime_lower_start(&harness, &root, prepared, source, permit),
    )
    .await
    .unwrap();
    assert!(
        denied
            .as_ref()
            .err()
            .is_some_and(|error| error.to_string().contains("owned_writer")),
        "unknown native store must fail before acquisition, with no ID-only fallback"
    );
    assert_eq!(
        store.inner.calls().await.create_thread,
        1,
        "native rejection must never invoke legacy create"
    );
    assert!(root.is_running(), "ordinary existing root remains usable");
    timeout(Duration::from_secs(10), root.shutdown_and_wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        harness
            .manager
            .remove_thread_if_matches(&root_id, &root)
            .await
            .is_some()
    );
    assert!(store.inner.calls().await.shutdown_thread >= 1);
    assert!(harness.manager.list_thread_ids().await.is_empty());
}

// Exercise the actual reserved-queue implementation with actual registered
// sessions. A cloned SessionIo replaces only the queue with a bounded observed
// queue, so no Core task can consume capacity before the publication barrier.
#[test_case::test_case(0; "prepared operation queue unchanged positive")]
#[test_case::test_case(1; "publication while queue capacity pending")]
#[test_case::test_case(2; "permission and environment publication while queue capacity pending")]
#[test_case::test_case(3; "no-op settings preserves pending operation")]
#[tokio::test]
async fn loaded_operation_publication_between_check_and_enqueue(change: u8) {
    let revoke = matches!(change, 1 | 2);
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    let target = loaded_custom_provider_resident(&harness, &parent).await;
    let target_id = target.session.thread_id;
    assert!(target.is_running());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));

    let guard = crate::agent::child_provider_authorization::current::prepare_loaded(
        &harness.control.runtime,
        target_id,
        parent_id,
    )
    .await
    .unwrap()
    .expect("ordinary delegated target is guarded");
    guard
        .check()
        .expect("prepared current helper stage is admitted");
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let mut io = target.io.clone();
    io.tx_sub = tx;
    io.submit(Op::Interrupt).await.unwrap();
    let submit =
        io.submit_with_trace_guarded(Op::Interrupt, None, None, None, None, None, Some(&guard));
    let mut submit = std::pin::pin!(submit);
    assert!(
        matches!(futures::poll!(submit.as_mut()), std::task::Poll::Pending),
        "the actual guarded queue must wait for capacity after the initial policy check"
    );
    if change == 2 {
        let environments = parent.session.inherited_environment_snapshot().await;
        let ready = environments.primary().expect("actual invoking executor");
        let mut owner = ready.config().clone();
        owner.permission_profile =
            codex_protocol::models::PermissionProfileSnapshot::legacy(PermissionProfile::Disabled);
        let mut selection = ready.selection();
        selection.config = EnvironmentConfigState::Ready(owner);
        parent
            .update_thread_settings(ThreadSettingsOverrides {
                permission_profile: Some(PermissionProfile::Disabled),
                environments: Some(TurnEnvironmentRequests::new(
                    parent.thread_settings_snapshot().await.cwd,
                    vec![selection.into_request()],
                )),
                ..Default::default()
            })
            .await
            .unwrap();
    } else if change == 3 {
        parent
            .update_thread_settings(ThreadSettingsOverrides::default())
            .await
            .unwrap();
    } else if revoke {
        let mut value: TomlValue =
            toml::from_str(&std::fs::read_to_string(&host_file).unwrap()).unwrap();
        value["agents"]["allowed_model_providers"] =
            TomlValue::Array(vec![TomlValue::String("openai".into())]);
        std::fs::write(&host_file, toml::to_string(&value).unwrap()).unwrap();
        let next = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(harness.config.codex_home.to_path_buf())
            .harness_overrides(crate::config::ConfigOverrides {
                cwd: Some(project.path().to_path_buf()),
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        assert_eq!(
            parent
                .refresh_runtime_config(parent.config().await, next)
                .await,
            crate::ConfigRefreshOutcome::Published
        );
    }
    assert!(matches!(rx.recv().await.unwrap().op, Op::Interrupt));
    let outcome = submit.await;
    if revoke {
        let error =
            outcome.expect_err("a prepared operation cannot cross a completed policy publication");
        assert!(loaded_fixture_policy_denial(&error.to_string()), "{error}");
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "the final admission must not enqueue the denied operation"
        );
        let mut later_stage_ran = false;
        assert!(
            guard
                .with_current(|| {
                    later_stage_ran = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(
            !later_stage_ran,
            "a later helper/retry/fallback stage cannot reuse the admitted earlier stage"
        );
    } else {
        outcome.expect("an unchanged prepared operation can enqueue after the capacity wait");
        assert!(matches!(rx.recv().await.unwrap().op, Op::Interrupt));
        guard
            .check()
            .expect("later stages remain usable under unchanged policy");
    }
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));
    target.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&target_id).await;
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
}

async fn loaded_custom_provider_resident_nested(
    harness: &AgentControlHarness,
    parent: &Arc<CodexThread>,
) -> Arc<CodexThread> {
    let runtime = &harness.control.runtime;
    let state = runtime.upgrade().unwrap();
    let membership = runtime.admit_start().unwrap();
    let slot = harness
        .control
        .reserve_v2_residency_slot(
            &state,
            &harness.config,
            &membership,
            /*protected_thread_id*/ None,
        )
        .await
        .unwrap();
    let turn = parent.session.new_default_turn().await;
    let step = StepContext::for_test(turn);
    let mut prepared = crate::agent::child_config::prepare_agent_spawn_config(
        &parent.session,
        &step,
        crate::agent::child_config::SpawnConfigOptions {
            resolved_provider: None,
            model_provider: Some("a"),
            version: crate::agent::child_config::SpawnConfigVersion::V2,
            full_history_fork: false,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("real trusted custom-provider preparation")
    .config;
    assert_eq!(prepared.model_provider_id, "a");
    let mut reservation = runtime.registry.reserve_spawn_slot(None).unwrap();
    let (source, mut metadata) = harness
        .control
        .prepare_thread_spawn(
            &mut reservation,
            &prepared,
            parent.session.thread_id,
            next_thread_spawn_depth(&parent.session_source),
            Some(
                AgentPath::from_string(format!(
                    "{}/nested_authority_target",
                    parent.session_source.get_agent_path().unwrap().as_str()
                ))
                .unwrap(),
            ),
            None,
            None,
        )
        .unwrap();
    let permit = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            runtime,
            parent.session.thread_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await
        .expect("consume actual prepared custom-provider proof");
    let child = fresh_lifetime_lower_start(harness, parent, prepared, source, permit)
        .await
        .unwrap();
    metadata.agent_id = Some(child.thread_id);
    reservation.commit(metadata);
    slot.commit(child.thread_id);
    // An idle lower-start has not activated its speculative rollout yet.
    // Materialize through the actual Session before the owner route reads it.
    child.thread.ensure_rollout_materialized().await;
    child.thread.flush_rollout().await.unwrap();
    assert_eq!(
        child.thread.multi_agent_version(),
        Some(MultiAgentVersion::V2)
    );
    let stored = child.thread.read_thread(true, true).await.unwrap();
    let history = stored
        .history
        .expect("actual lower-start persisted startup history");
    assert!(history.items.iter().any(|item| matches!(item,
        RolloutItem::SessionMeta(line) if line.meta.id == child.thread_id && line.meta.multi_agent_version == Some(MultiAgentVersion::V2)
    )), "actual persisted V2 metadata required by owner-loaded path");
    assert!(
        runtime
            .registry
            .agent_metadata_for_thread(child.thread_id)
            .is_some()
    );
    assert!(child.thread.session.active_turn.lock().await.is_none());
    assert_eq!(child.thread.config_snapshot().await.model_provider_id, "a");
    child.thread
}

#[tokio::test]
async fn inference_lineage_keeps_eviction_but_rejects_replaced_intermediate_registration() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    let target = loaded_custom_provider_resident(&harness, &parent).await;
    let target_id = target.session.thread_id;
    assert!(target.is_running());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));

    let owner = target;
    let owner_id = target_id;
    let descendant = loaded_custom_provider_resident_nested(&harness, &owner).await;
    let descendant_id = descendant.session.thread_id;
    let runtime = &harness.control.runtime;
    let check = || {
        runtime.with_current_native_operation(&parent.session, &descendant.session, &[], || Ok(()))
    };
    check().expect("current canonical lineage is admitted");
    let original = runtime.retained_authority(owner_id).unwrap();
    runtime.register_authority(
        crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
            &owner.session,
            &owner.session_source,
        )
        .await,
    );
    assert!(original.same_registration(&runtime.retained_authority(owner_id).unwrap()));
    check().expect("same-registration snapshot refresh preserves the ownership binding");
    let turn = owner.session.new_default_turn().await;
    owner
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                root_turn_id: None,
                turn_id: turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("owned intermediate completion".into()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    assert_eq!(
        harness
            .manager
            .try_evict_v2_thread(Arc::clone(&owner))
            .await
            .unwrap(),
        ThreadEvictionOutcome::Evicted
    );
    assert!(harness.manager.get_thread(owner_id).await.is_err());
    assert!(descendant.is_running());
    check().expect("completed own residency eviction preserves an active descendant");
    harness
        .control
        .ensure_child_loaded(parent_id, owner_id)
        .await
        .expect("private owned resume creates the actual replacement registration");
    let replacement = harness.manager.get_thread(owner_id).await.unwrap();
    assert!(!Arc::ptr_eq(&owner, &replacement));
    assert!(!original.same_registration(&runtime.retained_authority(owner_id).unwrap()));
    assert!(Arc::ptr_eq(
        &descendant,
        &harness.manager.get_thread(descendant_id).await.unwrap()
    ));
    assert!(
        descendant.is_running(),
        "the old descendant remains the exact Current allocation for this denial"
    );
    let before = harness.manager.captured_ops().len();
    let error = check()
        .expect_err("a Current descendant cannot acquire a replacement owner by stable thread ID");
    assert!(loaded_fixture_policy_denial(&error.to_string()), "{error}");
    assert_eq!(
        harness.manager.captured_ops().len(),
        before,
        "pure lineage denial performs no target submission/helper/network call"
    );
    descendant.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&descendant_id).await;
    replacement.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&owner_id).await;
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
}

#[test_case::test_case(false; "actual sender shares managed channel")]
#[test_case::test_case(true; "independent actual sender binding invalidates after precheck")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_operation_pins_every_actual_policy_owner_binding(independent: bool) {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let host_file = home.path().join("config.toml");
    let catalog_file = home.path().join("loaded-a.json");
    std::fs::write(
        &host_file,
        r#"
model = "gpt-5.5"
[agents]
max_depth = 8
allowed_model_providers = ["openai", "a"]
[agents.provider_defaults.a]
model = "shared-loaded"
model_catalog_json = "loaded-a.json"
[model_providers.a]
name = "Loaded custom fixture"
base_url = "http://127.0.0.1:9/v1"
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 1000
"#,
    )
    .unwrap();
    let mut info = codex_models_manager::bundled_models_response()
        .unwrap()
        .models
        .into_iter()
        .find(|info| info.slug == "gpt-5.5")
        .expect("real bundled model metadata");
    info.slug = "shared-loaded".into();
    info.multi_agent_version = Some(MultiAgentVersion::V2);
    info.supports_search_tool = false;
    info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    info.used_fallback_model_metadata = false;
    std::fs::write(
        &catalog_file,
        serde_json::to_vec(&serde_json::json!({"models":[info]})).unwrap(),
    )
    .unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(crate::config::ConfigOverrides {
            cwd: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .unwrap();
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    config.config_layer_stack = config
        .config_layer_stack
        .with_cloud_config_binding(snapshot.binding);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    assert_eq!(parent.config_snapshot().await.model_provider_id, "openai");
    let target = loaded_custom_provider_resident(&harness, &parent).await;
    let target_id = target.session.thread_id;
    assert!(target.is_running());
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));

    let caller_policy = codex_config::CloudConfigBundlePolicy::default();
    let mut caller_config = harness.config.clone();
    if independent {
        let mut snapshot = codex_config::CloudConfigBundleSnapshot {
            bundle: Ok(None),
            binding: None,
            config_server: None,
        };
        caller_policy.publish_snapshot(&mut snapshot);
        caller_config.config_layer_stack = caller_config
            .config_layer_stack
            .with_cloud_config_binding(snapshot.binding);
    }
    let caller = harness
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions::new(caller_config),
            harness.control.clone(),
        )
        .await
        .unwrap()
        .thread;
    let caller_id = caller.session.thread_id;
    assert_ne!(caller_id, parent_id);
    let mut operation = crate::agent::child_provider_authorization::current::prepare_loaded(
        &harness.control.runtime,
        target_id,
        caller_id,
    )
    .await
    .unwrap()
    .expect("actual delegated operation with actual cross-origin sender");
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    operation.set_after_policy_validation(Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .expect("bounded post-validation admission barrier");
    }));
    let callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let admitted = Arc::clone(&callbacks);
    let task = tokio::task::spawn_blocking(move || {
        operation.with_current(|| {
            admitted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("actual operation passes ALL policy prechecks before invalidation");
    let revision = *caller.session.child_policy_revision.lock().unwrap();
    if independent {
        let mut bundle = codex_config::CloudConfigBundle::default();
        bundle
            .config_toml
            .enterprise_managed
            .push(codex_config::CloudConfigFragment {
                id: "independent-sender-policy".into(),
                name: "sender revocation".into(),
                contents: "[agents]\nallowed_model_providers=['openai']\n".into(),
            });
        caller_policy.observe_remote_bundle(&bundle).unwrap();
        assert_ne!(
            caller
                .config()
                .await
                .config_layer_stack
                .cloud_config_binding()
                .unwrap()
                .read()
                .status,
            codex_config::CloudConfigBundleBindingStatus::Current
        );
    }
    assert_eq!(
        parent
            .config()
            .await
            .config_layer_stack
            .cloud_config_binding()
            .unwrap()
            .read()
            .status,
        codex_config::CloudConfigBundleBindingStatus::Current,
        "the target root binding stays current"
    );
    assert_eq!(
        *caller.session.child_policy_revision.lock().unwrap(),
        revision,
        "independent managed invalidation does not change the Session config publication revision"
    );
    let before = harness.manager.captured_ops().len();
    release_tx.send(()).unwrap();
    let outcome = task.await.unwrap();
    if independent {
        let error = outcome
            .expect_err("final managed admission must see the independent sender revocation");
        assert!(loaded_fixture_policy_denial(&error.to_string()), "{error}");
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    } else {
        outcome.expect("one shared policy channel may appear repeatedly without recursive locking");
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    }
    assert_eq!(harness.manager.captured_ops().len(), before);
    assert!(Arc::ptr_eq(
        &target,
        &harness.manager.get_thread(target_id).await.unwrap()
    ));
    caller.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&caller_id).await;
    target.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&target_id).await;
    parent.shutdown_and_wait().await.unwrap();
    harness.manager.remove_thread(&parent_id).await;
}
