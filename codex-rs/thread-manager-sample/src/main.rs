use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;
use std::sync::Arc;

use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use codex_core_api::AbsolutePathBuf;
use codex_core_api::AltScreenMode;
use codex_core_api::ApprovalsReviewer;
use codex_core_api::Arg0DispatchPaths;
use codex_core_api::AskForApproval;
use codex_core_api::AuthKeyringBackendKind;
use codex_core_api::AuthManager;
use codex_core_api::AutoCompactTokenLimitScope;
use codex_core_api::CodexAppsToolsCache;
use codex_core_api::CodexHomeUserInstructionsProvider;
use codex_core_api::CodexThread;
use codex_core_api::Config;
use codex_core_api::ConfigLayerStack;
use codex_core_api::ConfigLoadOptions;
use codex_core_api::ConfigRequirements;
use codex_core_api::ConfigRequirementsToml;
use codex_core_api::Constrained;
use codex_core_api::EnvironmentManager;
use codex_core_api::EventMsg;
use codex_core_api::ExecServerRuntimeOptions;
use codex_core_api::ExtensionRegistryBuilder;
use codex_core_api::Feature;
use codex_core_api::Features;
use codex_core_api::GhostSnapshotConfig;
use codex_core_api::History;
use codex_core_api::MemoriesConfig;
use codex_core_api::ModelAvailabilityNuxConfig;
use codex_core_api::MultiAgentV2Config;
use codex_core_api::NewThread;
use codex_core_api::Notice;
use codex_core_api::OPENAI_PROVIDER_ID;
use codex_core_api::OtelConfig;
use codex_core_api::PermissionProfile;
use codex_core_api::Permissions;
use codex_core_api::ProjectConfig;
use codex_core_api::RealtimeAudioConfig;
use codex_core_api::RealtimeConfig;
use codex_core_api::SessionPickerViewMode;
use codex_core_api::SessionSource;
use codex_core_api::SqliteConfig;
use codex_core_api::StartIfIdleSubmission;
use codex_core_api::StartThreadOptions;
use codex_core_api::TerminalResizeReflowConfig;
use codex_core_api::ThreadManager;
use codex_core_api::ThreadStoreConfig;
use codex_core_api::ToolSuggestConfig;
use codex_core_api::TuiKeymap;
use codex_core_api::TuiNotificationSettings;
use codex_core_api::TuiPetAnchor;
use codex_core_api::TurnInputRequest;
use codex_core_api::UriBasedFileOpener;
use codex_core_api::UserInput;
use codex_core_api::WebSearchMode;
use codex_core_api::arg0_dispatch_or_else;
use codex_core_api::bootstrap_auth_config;
use codex_core_api::build_models_manager;
use codex_core_api::built_in_model_providers;
use codex_core_api::find_codex_home;
use codex_core_api::init_state_db;
use codex_core_api::install_image_generation_extension;
use codex_core_api::item_event_to_server_notification;
use codex_core_api::load_config_toml_with_layer_stack;
use codex_core_api::local_agent_graph_store_from_state_db;
use codex_core_api::passthrough_image_store;
use codex_core_api::resolve_bootstrap_respect_system_proxy;
use codex_core_api::resolve_installation_id;
use codex_core_api::set_default_originator;
use codex_core_api::thread_store_from_config;

#[derive(Debug, Parser)]
#[command(
    name = "codex-thread-manager-sample",
    about = "Run one Codex turn through ThreadManager and print mapped notifications as newline-delimited JSON."
)]
struct Args {
    /// Override the model for this run.
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,

    /// Prompt text. If omitted, the prompt is read from piped stdin.
    #[arg(value_name = "PROMPT", num_args = 0.., trailing_var_arg = true)]
    prompt: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    arg0_dispatch_or_else(run_main)
}

async fn run_main(arg0_paths: Arg0DispatchPaths) -> anyhow::Result<()> {
    if let Err(err) = set_default_originator("codex_thread_manager_sample".to_string()) {
        tracing::warn!("failed to set originator: {err:?}");
    }

    let args = Args::parse();
    let prompt = if args.prompt.is_empty() {
        if std::io::stdin().is_terminal() {
            bail!("no prompt provided; pass a prompt argument or pipe one into stdin");
        }

        let mut prompt = String::new();
        std::io::stdin()
            .read_to_string(&mut prompt)
            .context("read prompt from stdin")?;
        let prompt = prompt.replace("\r\n", "\n").replace('\r', "\n");
        if prompt.trim().is_empty() {
            bail!("no prompt provided via stdin");
        }
        prompt
    } else {
        args.prompt.join(" ")
    };

    let config = new_config(args.model, arg0_paths).await?;
    let state_db = init_state_db(&config).await;

    let auth_manager =
        AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ false).await?;
    let local_runtime_paths = ExecServerRuntimeOptions::from_optional_paths(
        config.codex_self_exe.clone(),
        config.codex_linux_sandbox_exe.clone(),
    )?;
    let thread_store = thread_store_from_config(&config, state_db.clone());
    let environment_manager = Arc::new(
        EnvironmentManager::from_codex_home(
            config.codex_home.clone(),
            Some(local_runtime_paths),
            config.http_client_factory(),
        )
        .await?,
    );
    let installation_id = resolve_installation_id(&config.codex_home).await?;
    let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
        config.codex_home.clone(),
    ));
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    install_image_generation_extension(&mut extensions, auth_manager.clone(), |config: &Config| {
        Some(config.codex_home.clone())
    });
    let thread_manager = ThreadManager::new(
        &config,
        Arc::clone(&auth_manager),
        build_models_manager(&config, auth_manager),
        CodexAppsToolsCache::default(),
        SessionSource::Exec,
        environment_manager,
        Arc::new(extensions.build()),
        user_instructions_provider,
        /*analytics_events_client*/ None,
        passthrough_image_store(),
        Arc::clone(&thread_store),
        local_agent_graph_store_from_state_db(state_db.as_ref()),
        installation_id,
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );

    let NewThread {
        thread_id, thread, ..
    } = thread_manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .context("start Codex thread")?;

    let thread_id_string = thread_id.to_string();
    let turn_output = run_turn(&thread, &thread_id_string, prompt).await;
    let shutdown_result = thread.shutdown_and_wait().await;
    let _ = thread_manager.remove_thread(&thread_id).await;

    turn_output?;
    shutdown_result.context("shut down Codex thread")?;

    Ok(())
}

async fn new_config(
    model: Option<String>,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<Config> {
    let codex_home = find_codex_home().context("find Codex home")?;
    let cwd = AbsolutePathBuf::current_dir().context("resolve current directory")?;
    // Resolve auth through the shared bootstrap path so cargo-run builds retain the configured
    // secure store instead of the full Config loader's local-development file-store override.
    let bootstrap = load_config_toml_with_layer_stack(
        codex_home.as_path(),
        Some(&cwd),
        Vec::new(),
        ConfigLoadOptions::default(),
    )
    .await
    .context("load authentication configuration")?;
    let auth_config = bootstrap_auth_config(codex_home.as_path(), &bootstrap)?;
    let requirements = bootstrap.config_layer_stack.requirements();
    let requirements_toml = bootstrap.config_layer_stack.requirements_toml();
    // Preserve auth restrictions for every manager rebuilt from Config without importing
    // execution settings, plugins, or hooks from the bootstrap configuration.
    let config_layer_stack = ConfigLayerStack::new(
        Vec::new(),
        ConfigRequirements {
            allowed_login_methods: requirements.allowed_login_methods.clone(),
            allowed_chatgpt_workspaces: requirements.allowed_chatgpt_workspaces.clone(),
            ..Default::default()
        },
        ConfigRequirementsToml {
            allowed_login_methods: requirements_toml.allowed_login_methods.clone(),
            allowed_chatgpt_workspaces: requirements_toml.allowed_chatgpt_workspaces.clone(),
            ..Default::default()
        },
    )?;
    let mcp_oauth_credentials_store_mode = bootstrap
        .config_toml
        .mcp_oauth_credentials_store
        .unwrap_or_default();
    let respect_system_proxy = resolve_bootstrap_respect_system_proxy(
        &bootstrap.config_toml,
        bootstrap
            .config_layer_stack
            .requirements()
            .feature_requirements
            .as_ref(),
    )?;
    let model_provider_id = OPENAI_PROVIDER_ID.to_string();
    let model_providers = built_in_model_providers(/*openai_base_url*/ None);
    let model_provider = model_providers
        .get(&model_provider_id)
        .context("OpenAI model provider should be available")?
        .clone();

    // Start from the supported layer-free constructor so core initializes its private state.
    // Keep this sample's explicit settings below; only bootstrap auth may use configured layers.
    let mut config = Config::load_default_with_cli_overrides_for_codex_home(
        /*codex_home*/ codex_home.to_path_buf(),
        /*cli_overrides*/ Vec::new(),
    )
    .await
    .context("initialize minimal configuration")?;
    config.application_network_policy = Default::default();
    config.application_auth_route_config = Some(auth_config.auth_route_config.clone());
    config.config_layer_stack = config_layer_stack;
    config.plugins = Default::default();
    config.startup_warnings = Vec::new();
    config.bypass_hook_trust = false;
    config.model = model;
    config.daybreak_enabled = false;
    config.service_tier = None;
    config.review_model = None;
    config.model_context_window = None;
    config.model_auto_compact_token_limit = None;
    config.model_auto_compact_token_limit_scope = AutoCompactTokenLimitScope::Total;
    config.model_post_turn_compact_threshold_percent = 0;
    config.async_compaction_threshold_percent = 65;
    config.model_provider_id = model_provider_id;
    config.model_provider = model_provider;
    config.personality = None;
    config.permissions = Permissions::from_approval_and_profile(
        Constrained::allow_any(AskForApproval::Never),
        Constrained::allow_any(PermissionProfile::read_only()),
    )?;
    config.explicit_permission_profile_mode = false;
    config.custom_permission_profiles = Vec::new();
    config.approvals_reviewer = ApprovalsReviewer::User;
    config.enforce_residency = Constrained::allow_any(/*initial_value*/ None);
    config.hide_agent_reasoning = false;
    config.show_raw_agent_reasoning = false;
    config.base_instructions = None;
    config.base_instructions_provenance = None;
    config.developer_instructions = None;
    config.guardian_policy_config = None;
    config.guardian_extra_policy = None;
    config.guardian_policy_template = None;
    config.guardian_transcript_mode = Default::default();
    config.guardian_conversation_history_prompt = None;
    config.guardian_conversation_history_max_output_tokens = None;
    config.guardian_circuit_break_action = Default::default();
    config.include_permissions_instructions = false;
    config.include_apps_instructions = false;
    config.include_collaboration_mode_instructions = false;
    config.include_skill_instructions = false;
    config.skill_max_context_tokens = None;
    config.cloud_skill_enabled = false;
    config.orchestrator_mcp_enabled = false;
    config.include_environment_context = false;
    config.include_environment_context_time = true;
    config.compact_prompt = None;
    config.notify = None;
    config.tui_notifications = TuiNotificationSettings::default();
    config.animations = true;
    config.tui_effects = Default::default();
    config.tui_rendering = Default::default();
    config.show_tooltips = true;
    config.tui_show_server_version_notice = true;
    config.tui_auto_recap = true;
    config.model_availability_nux = ModelAvailabilityNuxConfig::default();
    config.tui_fullscreen_transcript = false;
    config.tui_mouse_scroll_speed = None;
    config.tui_copy_on_select = Default::default();
    config.tui_copy_on_select_clear_selection = false;
    config.tui_right_click_paste = Default::default();
    config.tui_alternate_screen = AltScreenMode::Auto;
    config.tui_status_line = None;
    config.tui_status_line_use_colors = true;
    config.tui_terminal_title = None;
    config.tui_theme = None;
    config.tui_raw_output_mode = false;
    config.tui_pet = None;
    config.tui_pet_anchor = TuiPetAnchor::Composer;
    config.terminal_resize_reflow = TerminalResizeReflowConfig::default();
    config.tui_keymap = TuiKeymap::default();
    config.tui_quick_dials = BTreeMap::new();
    config.tui_session_picker_view = SessionPickerViewMode::Dense;
    config.tui_agents_overview_grouping = Default::default();
    config.tui_resume_cwd = None;
    config.tui_vim_mode_default = false;
    config.tui_question_esc_back = true;
    config.cwd = cwd.clone();
    config.workspace_roots = vec![cwd];
    config.workspace_roots_explicit = false;
    config.cli_auth_credentials_store_mode = auth_config.auth_credentials_store_mode;
    config.mcp_servers = Constrained::allow_any(HashMap::new());
    config.mcp_enterprise_managed_auth = None;
    config.non_prefixed_mcp_tool_servers = None;
    config.mcp_oauth_credentials_store_mode = mcp_oauth_credentials_store_mode;
    config.mcp_oauth_callback_port = None;
    config.mcp_oauth_callback_url = None;
    config.mcp_optional_startup_grace = std::time::Duration::from_secs(1);
    config.model_providers = model_providers;
    config.project_doc_max_bytes = 32 * 1024;
    config.project_doc_fallback_filenames = Vec::new();
    config.tool_output_token_limit = None;
    config.agents_enabled = true;
    config.agent_max_threads = Some(6);
    config.agent_default_subagent_model = None;
    config.agent_default_subagent_reasoning_effort = None;
    config.agent_interrupt_message_enabled = false;
    config.agent_max_depth = 1;
    config.agent_roles = BTreeMap::new();
    config.memories = MemoriesConfig::default();
    config.sqlite = SqliteConfig::from_sqlite_home(codex_home.clone());
    config.log_dir = codex_home.join("log").to_path_buf();
    config.codex_home = codex_home;
    config.history = History::default();
    config.ephemeral = true;
    config.extra_config = None;
    config.file_opener = UriBasedFileOpener::VsCode;
    config.codex_self_exe = arg0_paths.codex_self_exe;
    config.codex_linux_sandbox_exe = arg0_paths.codex_linux_sandbox_exe;
    config.main_execve_wrapper_exe = None;
    config.zsh_path = None;
    config.model_reasoning_effort = None;
    config.plan_mode_reasoning_effort = None;
    config.model_reasoning_summary = None;
    config.model_catalog = None;
    config.model_verbosity = None;
    config.chatgpt_base_url = auth_config
        .chatgpt_base_url
        .clone()
        .unwrap_or_else(|| "https://chatgpt.com/backend-api/".to_string());
    config.respect_system_proxy = respect_system_proxy;
    config.apps_mcp_product_sku = None;
    config.responses_api_metadata = BTreeMap::new();
    config.realtime_audio = RealtimeAudioConfig::default();
    config.experimental_realtime_ws_base_url = None;
    config.experimental_realtime_webrtc_call_base_url = None;
    config.experimental_realtime_ws_model = None;
    config.realtime = RealtimeConfig::default();
    config.experimental_realtime_ws_backend_prompt = None;
    config.experimental_realtime_ws_startup_context = None;
    config.experimental_realtime_start_instructions = None;
    config.experimental_thread_store = ThreadStoreConfig::Local;
    config.forced_chatgpt_workspace_id = auth_config.forced_chatgpt_workspace_id.clone();
    config.forced_login_method = auth_config.forced_login_method;
    config.web_search_mode = Constrained::allow_any(WebSearchMode::Disabled);
    config.web_search_config = None;
    config.experimental_request_user_input_enabled = true;
    config.update_plan_enabled = true;
    config.tool_registry = Default::default();
    config.code_mode = Default::default();
    config.background_terminal_max_timeout = 300_000;
    config.thread_unload_delay = std::time::Duration::from_secs(/*secs*/ 1800);
    config.ghost_snapshot = GhostSnapshotConfig::default();
    config.multi_agent_v2 = MultiAgentV2Config::default();
    config.max_goal_token_budget = None;
    config.token_budget = None;
    config.token_budget_startup_config = None;
    config.rollout_budget = None;
    config.current_time_reminder = None;
    config.sleep_tool_mode = Default::default();
    config.features = Default::default();
    config.prefer_mxc = false;
    config.runtime_feature_defaults = Default::default();
    config.suppress_unstable_features_warning = false;
    config.active_project = ProjectConfig { trust_level: None };
    config.notices = Notice::default();
    config.check_for_update_on_startup = false;
    config.disable_paste_burst = false;
    config.analytics_enabled = Some(false);
    config.feedback_enabled = false;
    config.tool_suggest = ToolSuggestConfig::default();
    config.otel = OtelConfig::default();
    config
        .features
        .set(Features::with_defaults())
        .context("configure default features")?;
    // AuthManager and MCP both derive their credential backend from Config.
    config.features.set_enabled(
        Feature::SecretAuthStorage,
        auth_config.keyring_backend_kind == AuthKeyringBackendKind::Secrets,
    )?;
    Ok(config)
}

async fn run_turn(thread: &CodexThread, thread_id: &str, prompt: String) -> anyhow::Result<()> {
    let submission = thread
        .start_turn_if_idle(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt,
            text_elements: Vec::new(),
        }]))
        .await
        .context("submit user input")?;
    if let StartIfIdleSubmission::NotSubmitted { reason } = submission {
        bail!("turn input was not submitted: {reason:?}");
    }

    let mut current_turn_id: Option<String> = None;
    let mut stdout = std::io::stdout().lock();
    loop {
        let event = thread.next_event().await.context("read Codex event")?;
        let notification = match &event.msg {
            EventMsg::TurnStarted(event) => {
                current_turn_id = Some(event.turn_id.clone());
                None
            }
            EventMsg::DynamicToolCallResponse(_)
            | EventMsg::McpToolCallBegin(_)
            | EventMsg::McpToolCallEnd(_)
            | EventMsg::CollabAgentSpawnBegin(_)
            | EventMsg::CollabAgentSpawnEnd(_)
            | EventMsg::CollabAgentInteractionBegin(_)
            | EventMsg::CollabAgentInteractionEnd(_)
            | EventMsg::CollabWaitingBegin(_)
            | EventMsg::CollabWaitingEnd(_)
            | EventMsg::CollabCloseBegin(_)
            | EventMsg::CollabCloseEnd(_)
            | EventMsg::CollabResumeBegin(_)
            | EventMsg::CollabResumeEnd(_)
            | EventMsg::SubAgentActivity(_)
            | EventMsg::AgentMessageContentDelta(_)
            | EventMsg::PlanDelta(_)
            | EventMsg::ReasoningContentDelta(_)
            | EventMsg::ReasoningRawContentDelta(_)
            | EventMsg::AgentReasoningSectionBreak(_)
            | EventMsg::ItemStarted(_)
            | EventMsg::ItemCompleted(_)
            | EventMsg::PatchApplyBegin(_)
            | EventMsg::PatchApplyUpdated(_)
            | EventMsg::TerminalInteraction(_)
            | EventMsg::ExecCommandBegin(_)
            | EventMsg::ExecCommandOutputDelta(_)
            | EventMsg::ExecCommandEnd(_) => Some(item_event_to_server_notification(
                event.msg.clone(),
                thread_id,
                current_turn_id
                    .as_deref()
                    .context("mapped notification arrived before turn started")?,
            )),
            _ => None,
        };
        if let Some(notification) = notification {
            serde_json::to_writer(&mut stdout, &notification)
                .context("serialize mapped notification")?;
            stdout
                .write_all(b"\n")
                .context("write notification newline")?;
            stdout.flush().context("flush notification output")?;
        }

        match event.msg {
            EventMsg::TurnComplete(_) => {
                return Ok(());
            }
            EventMsg::Error(event) => {
                bail!(event.message);
            }
            EventMsg::TurnAborted(_) => {
                bail!("turn aborted");
            }
            EventMsg::ExecApprovalRequest(_) => {
                bail!("turn requested exec approval");
            }
            EventMsg::ApplyPatchApprovalRequest(_) => {
                bail!("turn requested patch approval");
            }
            EventMsg::RequestPermissions(_) => {
                bail!("turn requested permissions");
            }
            EventMsg::RequestUserInput(_) => {
                bail!("turn requested user input");
            }
            EventMsg::DynamicToolCallRequest(_) => {
                bail!("turn requested a dynamic tool call");
            }
            _ => {}
        }
    }
}
