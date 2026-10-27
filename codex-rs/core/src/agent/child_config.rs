//! Prepares child configuration from captured step settings and requested overrides.
//!
//! Spawn and reload share live runtime policy; role and model precedence, full-history
//! inheritance, and validation messages remain the same for each multi-agent version.

use crate::agent::child_provider_authorization::ChildProviderScope;
use crate::agent::child_provider_authorization::PreparedChildAuthorization;
#[cfg(test)]
use crate::agent::child_provider_authorization::current_policy;
use crate::agent::child_provider_authorization::current_policy_in_scope;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::PreparedAgentRole;
use crate::agent::role::prepare_role;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_config::child_provider::ChildProviderRequest;
use codex_config::child_provider::ResolvedChildProvider;
use codex_features::Feature;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::models::BaseInstructions;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::MultiAgentVersion;
use std::sync::Arc;

pub(crate) const MAX_SPAWN_AGENT_MODEL_OVERRIDES: usize = 5;

pub(crate) fn model_supports_multi_agent_backend(
    model: &ModelPreset,
    multi_agent_version: MultiAgentVersion,
) -> bool {
    multi_agent_version != MultiAgentVersion::V2
        || model.multi_agent_version != Some(MultiAgentVersion::Disabled)
}

/// Selects the existing spawn tool's role-inheritance rules.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnConfigVersion {
    V1,
    V2,
}

pub(crate) struct SpawnConfigOptions<'a> {
    /// Already authorized by the shared resolver, including history and origin checks.
    pub(crate) resolved_provider: Option<&'a ResolvedChildProvider>,
    pub(crate) model_provider: Option<&'a str>,
    pub(crate) version: SpawnConfigVersion,
    pub(crate) full_history_fork: bool,
    pub(crate) role_name: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

/// Binds a validated manager to the exact selected configuration until startup.
#[derive(Debug, Clone)]
pub(crate) struct PreparedChildModels {
    provider_id: String,
    model: Option<String>,
    provider: codex_model_provider_info::ModelProviderInfo,
    catalog: Option<codex_protocol::openai_models::ModelsResponse>,
    manager: SharedModelsManager,
    auth_manager: Arc<codex_login::AuthManager>,
}

impl PartialEq for PreparedChildModels {
    fn eq(&self, other: &Self) -> bool {
        self.provider_id == other.provider_id
            && self.model == other.model
            && self.provider == other.provider
            && self.catalog == other.catalog
            && Arc::ptr_eq(&self.manager, &other.manager)
            && Arc::ptr_eq(&self.auth_manager, &other.auth_manager)
    }
}

impl PreparedChildModels {
    pub(crate) fn manager(&self) -> &SharedModelsManager {
        &self.manager
    }
}

pub(crate) fn take_prepared_child_models(
    config: &mut Config,
    auth_manager: &Arc<codex_login::AuthManager>,
) -> Result<Option<SharedModelsManager>, String> {
    let Some(prepared) = config.prepared_child_models.take() else {
        return Ok(None);
    };
    if prepared.provider_id != config.model_provider_id
        || prepared.model != config.model
        || prepared.provider != config.model_provider
        || prepared.catalog != config.model_catalog
        || !Arc::ptr_eq(&prepared.auth_manager, auth_manager)
    {
        return Err(
            "child_provider_model_unavailable: prepared catalog scope changed before startup"
                .into(),
        );
    }
    Ok(Some(prepared.manager))
}

pub(crate) struct PreparedSpawnConfig {
    pub(crate) config: Config,
    pub(crate) role_name: Option<String>,
}

/// Resolves child settings before starting the thread, retaining the invoking tool's precedence.
pub(crate) async fn prepare_agent_spawn_config(
    session: &Session,
    step_context: &StepContext,
    options: SpawnConfigOptions<'_>,
) -> Result<PreparedSpawnConfig, String> {
    let turn = step_context.turn.as_ref();
    let mut config =
        build_agent_spawn_config(&session.get_base_instructions().await, step_context)?;
    if options.version == SpawnConfigVersion::V1 && options.full_history_fork {
        reject_full_fork_agent_type_override(options.role_name)?;
    }
    let (parent_base, parent_settings, current_environments) =
        session.native_agent_authority_snapshot().await;
    if parent_base.config_layer_stack != config.config_layer_stack
        || parent_settings.approval_policy != config.permissions.approval_policy.value()
        || parent_settings.approvals_reviewer != config.approvals_reviewer
    {
        return Err(
            "child_provider_policy_conflict: invoking runtime scope changed before preparation"
                .into(),
        );
    }
    let execution_scope = ChildProviderScope::from_step(&config, &step_context.environments);
    if execution_scope
        != ChildProviderScope::from_current(&parent_base, &parent_settings, &current_environments)
    {
        return Err("child_provider_policy_conflict: invoking execution environment authority changed before preparation".into());
    }
    let prepared_role = if !options.full_history_fork
        || (options.version == SpawnConfigVersion::V2 && options.role_name.is_some())
    {
        Some(prepare_role(&config, options.role_name).await?)
    } else {
        None
    };
    // Role loading is asynchronous. Recheck the actual host scope before any
    // target catalog/helper work rather than blessing a stale pre-role snapshot.
    let (current_base, current_settings, current_environments) =
        session.native_agent_authority_snapshot().await;
    if current_settings != parent_settings
        || current_base.as_ref() != parent_base.as_ref()
        || execution_scope
            != ChildProviderScope::from_current(
                &current_base,
                &current_settings,
                &current_environments,
            )
    {
        return Err(
            "child_provider_policy_conflict: host authority changed while loading the child role"
                .into(),
        );
    }
    let policy = current_policy_in_scope(&config, &execution_scope).map_err(|_| {
        "child_provider_policy_conflict: runtime policy cannot be established".to_owned()
    })?;
    let origin = session
        .services
        .local_agent_runtime
        .child_provider_origins
        .verified(session.thread_id);
    let explicit_provider = options.model_provider.or_else(|| {
        options
            .resolved_provider
            .map(|resolved| resolved.provider_id.as_str())
    });
    let role_provider = prepared_role
        .as_ref()
        .and_then(|role| role.model_provider.as_deref());
    let resolved = policy
        .resolve(ChildProviderRequest {
            explicit_provider,
            role_provider,
            parent_provider_id: &config.model_provider_id,
            parent_provider: &config.model_provider,
            parent_model: config
                .model
                .as_deref()
                .unwrap_or(&step_context.settings.model_info.slug),
            authorization_origin: origin.clone(),
            explicit_model: options.model.or_else(|| {
                options
                    .resolved_provider
                    .map(|resolved| resolved.model.as_str())
            }),
            role_model: prepared_role.as_ref().and_then(PreparedAgentRole::model),
            default_subagent_model: config.agent_default_subagent_model.as_deref(),
            full_history: options.full_history_fork,
            host_owned: false,
        })
        .map_err(|error| error.to_string())?;
    if let Some(provided) = options.resolved_provider
        && (provided.provider != resolved.provider
            || provided.model_catalog_json != resolved.model_catalog_json)
    {
        return Err("child_provider_policy_conflict: supplied provider selection differs from current trusted policy".into());
    }
    // Resolve and authorize every launch, while preserving the legacy catalog path
    // for ordinary inheritance of the host-captured root provider.
    let strict_catalog = explicit_provider.is_some()
        || role_provider.is_some()
        || policy.default_provider().is_some()
        || resolved.changed_provider
        || origin
            .as_ref()
            .map(codex_config::child_provider::VerifiedChildProviderOrigin::provider_id)
            != Some(resolved.provider_id.as_str());
    config.child_provider_policy = policy;
    let selected_manager = if strict_catalog {
        let resolved = &resolved;

        if options.full_history_fork
            && (resolved.changed_provider
                || resolved.provider_id != config.model_provider_id
                || resolved.provider != config.model_provider)
        {
            return Err(
                "child_provider_history_unsupported: use a fresh child when changing provider"
                    .into(),
            );
        }
        if !options.full_history_fork {
            // Retain an explicit host instruction override, but do not freeze
            // instructions resolved from the parent model catalog.
            config.base_instructions = turn.config.base_instructions.clone();
            config.base_instructions_provenance = turn.config.base_instructions_provenance.clone();
            if matches!(
                config.base_instructions_provenance,
                Some(codex_protocol::models::BaseInstructionsProvenance::Model { .. })
            ) {
                config.base_instructions = None;
                config.base_instructions_provenance = None;
            }
        }
        Some(
            prepare_resolved_child_models(
                session,
                &mut config,
                resolved,
                options.model,
                options.reasoning_effort.clone(),
            )
            .await?,
        )
    } else {
        None
    };
    let models_manager = selected_manager
        .as_ref()
        .unwrap_or(&session.services.models_manager);
    if !strict_catalog {
        apply_requested_spawn_agent_model_overrides(
            session,
            step_context,
            &mut config,
            options.model,
            options.reasoning_effort,
        )
        .await?;
    }
    let mut role_service_tier = None;
    if let Some(role) = prepared_role.as_ref() {
        role_service_tier = apply_spawn_agent_role(models_manager, &mut config, role)
            .await
            .map_err(|err| {
                if strict_catalog && err.starts_with("Reasoning effort") {
                    format!("child_provider_capability_unsupported: {err}")
                } else {
                    err
                }
            })?;
        if options.version == SpawnConfigVersion::V2
            && options.full_history_fork
            && config.developer_instructions.is_none()
        {
            config
                .developer_instructions
                .clone_from(&turn.developer_instructions);
        }
    }
    if strict_catalog {
        let resolved = &resolved;
        validate_resolved_child_model(models_manager, &config, resolved, turn.multi_agent_version)
            .await?;
    }
    if strict_catalog && role_service_tier.is_some() {
        config.child_role_service_tier_override = true;
        if let Some(tier) = role_service_tier.as_deref()
            && tier != SERVICE_TIER_DEFAULT_REQUEST_VALUE
        {
            let tier = codex_protocol::config_types::ServiceTier::from_request_value(tier)
                .map(|tier| tier.request_value().to_string())
                .unwrap_or_else(|| tier.to_owned());
            if config.service_tier.is_none() || !config.features.service_tier_enabled(&tier) {
                return Err(format!(
                    "child_provider_capability_unsupported: service tier `{tier}` is disabled"
                ));
            }
            let model = config
                .model
                .as_deref()
                .ok_or("child_provider_model_required")?;
            let info = models_manager
                .get_model_info(model, &config.to_models_manager_config())
                .await;
            if !model_supports_selected_tier(&config, &info, &tier) {
                return Err(format!(
                    "child_provider_capability_unsupported: service tier `{tier}` is not supported by model `{model}`"
                ));
            }
        }
    } else {
        apply_spawn_agent_service_tier_with_manager(
            session,
            models_manager,
            &mut config,
            strict_catalog,
        )
        .await?;
    }
    apply_spawn_agent_runtime_overrides(&mut config, turn)?;
    apply_step_security_overrides(&mut config, step_context)?;

    // Remember an applied configured default so cold reload reapplies its restrictions.
    let role_name = options
        .role_name
        .or_else(|| {
            (options.version == SpawnConfigVersion::V2
                && !options.full_history_fork
                && config
                    .agent_roles
                    .get(DEFAULT_ROLE_NAME)
                    .is_some_and(|role| role.config_file.is_some()))
            .then_some(DEFAULT_ROLE_NAME)
        })
        .map(str::to_owned);
    config.prepared_child_authorization = Some(PreparedChildAuthorization::capture(
        session,
        &config,
        options.full_history_fork,
        parent_settings,
        parent_base,
        role_name.clone(),
        execution_scope,
    ));
    Ok(PreparedSpawnConfig { config, role_name })
}

pub(crate) async fn prepare_resolved_child_models(
    session: &Session,
    config: &mut Config,
    resolved: &ResolvedChildProvider,
    requested_model: Option<&str>,
    requested_effort: Option<ReasoningEffort>,
) -> Result<SharedModelsManager, String> {
    let definition_changed = config.model_provider != resolved.provider;
    config.model_provider_id = resolved.provider_id.clone();
    config.model_provider = resolved.provider.clone();
    if resolved.changed_provider || definition_changed {
        // These are parent model overrides, not target provider defaults.
        config.model_context_window = None;
        config.model_auto_compact_token_limit = None;
        config.model_reasoning_summary = None;
        config.model_verbosity = None;
        config.model_catalog = None;
        config.service_tier = None;
    }
    if let Some(path) = &resolved.model_catalog_json {
        let bytes = tokio::fs::read(path).await.map_err(|_| {
            format!(
                "child_provider_model_unavailable: provider `{}` catalog cannot be read",
                resolved.provider_id
            )
        })?;
        config.model_catalog = Some(serde_json::from_slice(&bytes).map_err(|_| {
            format!(
                "child_provider_model_unavailable: provider `{}` catalog is invalid",
                resolved.provider_id
            )
        })?);
    }
    let manager = crate::thread_manager::build_models_manager(
        config,
        Arc::clone(&session.services.auth_manager),
    );
    let catalog = manager
        .raw_model_catalog(
            RefreshStrategy::OnlineIfUncached,
            config.http_client_factory(),
        )
        .await;
    let info = resolved
        .validate_model(&config.model_provider_id, &catalog.models)
        .map_err(|err| err.to_string())?;
    let effort = requested_effort
        .clone()
        .or_else(|| config.agent_default_subagent_reasoning_effort.clone());
    if resolved.changed_provider {
        // An unsupported inherited default may be omitted; an explicit effort may not.
        config.model_reasoning_effort = match effort {
            Some(effort) if requested_effort.is_some() => Some(effort),
            Some(effort)
                if info
                    .supported_reasoning_levels
                    .iter()
                    .any(|preset| preset.effort == effort) =>
            {
                Some(effort)
            }
            _ => info.default_reasoning_level.clone(),
        };
    } else if let Some(selected) =
        requested_model.or(config.agent_default_subagent_model.as_deref())
    {
        let selected = catalog.models.iter().find(|model| model.slug == selected && !model.used_fallback_model_metadata)
            .ok_or_else(|| format!("child_provider_model_unavailable: model `{selected}` is absent from the selected catalog"))?;
        if let Some(effort) = effort {
            validate_spawn_agent_reasoning_effort(
                &selected.slug,
                &selected.supported_reasoning_levels,
                &effort,
            )?;
            config.model_reasoning_effort = Some(effort);
        } else {
            config.model_reasoning_effort = selected.default_reasoning_level.clone();
        }
    } else if let Some(effort) = effort {
        validate_spawn_agent_reasoning_effort(
            &resolved.model,
            &info.supported_reasoning_levels,
            &effort,
        )?;
        config.model_reasoning_effort = Some(effort);
    }
    config.model = Some(resolved.model.clone());
    config.prepared_child_models = Some(PreparedChildModels {
        provider_id: config.model_provider_id.clone(),
        model: config.model.clone(),
        provider: config.model_provider.clone(),
        catalog: config.model_catalog.clone(),
        manager: Arc::clone(&manager),
        auth_manager: Arc::clone(&session.services.auth_manager),
    });
    Ok(manager)
}

pub(crate) async fn validate_resolved_child_model(
    manager: &SharedModelsManager,
    config: &Config,
    resolved: &ResolvedChildProvider,
    multi_agent_version: MultiAgentVersion,
) -> Result<(), String> {
    if config.model.as_deref() != Some(resolved.model.as_str()) {
        return Err(
            "child_provider_model_unavailable: role model changed after provider resolution".into(),
        );
    }
    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Offline, config.http_client_factory())
        .await;
    let available = manager.build_available_models(catalog.models.clone());
    if !available.iter().any(|model| model.model == resolved.model) {
        return Err("child_provider_model_unavailable: selected model is not available for the provider authentication mode".into());
    }
    let info = resolved
        .validate_model(&config.model_provider_id, &catalog.models)
        .map_err(|err| err.to_string())?;
    if info.multi_agent_version == Some(MultiAgentVersion::Disabled)
        && multi_agent_version == MultiAgentVersion::V2
    {
        return Err("child_provider_capability_unsupported: selected model does not support the multi-agent backend".into());
    }
    if let Some(effort) = &config.model_reasoning_effort {
        validate_spawn_agent_reasoning_effort(
            &resolved.model,
            &info.supported_reasoning_levels,
            effort,
        )
        .map_err(|err| format!("child_provider_capability_unsupported: {err}"))?;
    }
    Ok(())
}

/// Builds the base config snapshot for a newly spawned sub-agent.
///
/// The returned config starts from the parent's effective config and then refreshes the
/// model selection and reasoning settings captured for the invoking step, plus the turn's
/// runtime approval policy, sandbox, and cwd. Role-specific overrides are layered
/// after this step; skipping this helper and cloning stale config state directly can send the child
/// agent out with the wrong provider or runtime policy.
pub(crate) fn build_agent_spawn_config(
    base_instructions: &BaseInstructions,
    step_context: &StepContext,
) -> Result<Config, String> {
    let mut config = build_agent_shared_config(step_context.turn.as_ref())?;
    let settings = &step_context.settings;
    apply_step_security_overrides(&mut config, step_context)?;
    config.model = Some(settings.model_info.slug.clone());
    // Retain model-default intent instead of turning an unspecified selection
    // into the current model's resolved default. Transport/status resolve it
    // against the selected catalog; changed-provider preparation sets its own
    // target default and explicit/role overrides still apply below.
    config.model_reasoning_effort = settings.reasoning_effort().cloned();
    config.model_reasoning_summary = Some(settings.reasoning_summary);
    config.base_instructions = Some(base_instructions.text.clone());
    config.base_instructions_provenance = base_instructions.provenance.clone();
    Ok(config)
}

pub(crate) fn build_agent_resume_config(turn: &TurnContext) -> Result<Config, String> {
    let mut config = build_agent_shared_config(turn)?;
    // For resume, keep base instructions sourced from rollout/session metadata.
    config.base_instructions = None;
    config.base_instructions_provenance = None;
    Ok(config)
}

fn build_agent_shared_config(turn: &TurnContext) -> Result<Config, String> {
    let base_config = turn.config.clone();
    let mut config = (*base_config).clone();
    config.prepared_child_models = None;
    config.prepared_child_authorization = None;
    config.child_role_service_tier_override = false;
    if config
        .features
        .enabled(Feature::SubagentDefaultContextLimits)
    {
        config.model_context_window = None;
        config.model_auto_compact_token_limit = None;
    }
    // Preserve activation for history forks without freezing the parent's model-owned prompts.
    // Fresh child startup restores configured preferences from the retained snapshot.
    config.token_budget = turn.configured_token_budget.clone();
    config.model = Some(turn.model_info().slug.clone());
    config.model_provider = turn.provider.info().clone();
    config.model_reasoning_effort = turn
        .reasoning_effort()
        .or(turn.model_info().default_reasoning_level.as_ref())
        .cloned();
    config.model_reasoning_summary = Some(turn.reasoning_summary());
    config.developer_instructions = turn.developer_instructions.clone();
    if turn.multi_agent_version == MultiAgentVersion::V2
        && let Some(developer_instructions) = turn
            .config
            .multi_agent_v2
            .subagent_developer_instructions
            .clone()
    {
        config.developer_instructions = Some(developer_instructions);
    }
    apply_spawn_agent_runtime_overrides(&mut config, turn)?;

    Ok(config)
}

fn reject_full_fork_agent_type_override(agent_type: Option<&str>) -> Result<(), String> {
    if agent_type.is_some() {
        return Err(
            "Full-history forked agents inherit the parent agent type; omit agent_type, or spawn without a full-history fork.".to_string());
    }
    Ok(())
}

/// Copies runtime-only turn state onto a child config before it is handed to `AgentControl`.
///
/// These values are chosen by the live turn rather than persisted config, so leaving them stale can
/// make a child agent disagree with its parent about approval policy, cwd, or sandboxing.
// Native launches are step-scoped: TurnContext deliberately retains the
// initial-turn approval values and cannot authorize a later step's child.
fn apply_step_security_overrides(config: &mut Config, step: &StepContext) -> Result<(), String> {
    config
        .permissions
        .approval_policy
        .set(step.settings.approval_policy())
        .map_err(|error| format!("approval_policy is invalid: {error}"))?;
    config.approvals_reviewer = step.settings.approvals_reviewer();
    Ok(())
}

fn apply_spawn_agent_runtime_overrides(
    config: &mut Config,
    turn: &TurnContext,
) -> Result<(), String> {
    config
        .permissions
        .approval_policy
        .set(turn.approval_policy())
        .map_err(|err| format!("approval_policy is invalid: {err}"))?;
    config.approvals_reviewer = turn.config.approvals_reviewer;
    #[allow(deprecated)]
    let turn_cwd = turn.cwd.clone();
    config.cwd = turn_cwd;
    config
        .permissions
        .set_permission_profile_from_session_snapshot(
            turn.config
                .permissions
                .permission_profile_state()
                .snapshot(),
        )
        .map_err(|err| format!("permission_profile is invalid: {err}"))?;
    Ok(())
}

async fn apply_requested_spawn_agent_model_overrides(
    session: &Session,
    step_context: &StepContext,
    config: &mut Config,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), String> {
    let turn = step_context.turn.as_ref();
    let requested_model = requested_model.or(turn.config.agent_default_subagent_model.as_deref());
    let requested_reasoning_effort = requested_reasoning_effort
        .or_else(|| turn.config.agent_default_subagent_reasoning_effort.clone());
    if requested_model.is_none() && requested_reasoning_effort.is_none() {
        return Ok(());
    }

    if let Some(requested_model) = requested_model {
        let available_models = session
            .services
            .models_manager
            .list_models(RefreshStrategy::Offline, config.http_client_factory())
            .await;
        let selected_model_name = find_spawn_agent_model_name(
            &available_models,
            requested_model,
            turn.multi_agent_version,
        )?;
        let selected_model_info = session
            .services
            .models_manager
            .get_model_info(&selected_model_name, &config.to_models_manager_config())
            .await;

        config.model = Some(selected_model_name.clone());
        if let Some(reasoning_effort) = requested_reasoning_effort {
            validate_spawn_agent_reasoning_effort(
                &selected_model_name,
                &selected_model_info.supported_reasoning_levels,
                &reasoning_effort,
            )?;
            config.model_reasoning_effort = Some(reasoning_effort);
        } else {
            config.model_reasoning_effort = selected_model_info.default_reasoning_level;
        }

        return Ok(());
    }

    if let Some(reasoning_effort) = requested_reasoning_effort {
        validate_spawn_agent_reasoning_effort(
            &step_context.settings.model_info.slug,
            &step_context.settings.model_info.supported_reasoning_levels,
            &reasoning_effort,
        )?;
        config.model_reasoning_effort = Some(reasoning_effort);
    }

    Ok(())
}

#[cfg(test)]
pub(crate) async fn apply_spawn_agent_service_tier(
    session: &Session,
    config: &mut Config,
) -> Result<(), String> {
    apply_spawn_agent_service_tier_with_manager(
        session,
        &session.services.models_manager,
        config,
        /*strict_catalog*/ false,
    )
    .await
}

async fn apply_spawn_agent_service_tier_with_manager(
    session: &Session,
    models_manager: &SharedModelsManager,
    config: &mut Config,
    strict_catalog: bool,
) -> Result<(), String> {
    let Some(service_tier) = session.services.agent_control.service_tier() else {
        config.service_tier = None;
        return Ok(());
    };
    if service_tier == SERVICE_TIER_DEFAULT_REQUEST_VALUE {
        config.service_tier = Some(service_tier);
        return Ok(());
    }

    let model = config.model.clone().ok_or_else(|| {
        "spawn_agent could not resolve the child model for service tier validation".to_string()
    })?;
    let model_info = models_manager
        .get_model_info(model.as_str(), &config.to_models_manager_config())
        .await;

    let supported = if strict_catalog {
        model_supports_selected_tier(config, &model_info, &service_tier)
    } else {
        model_info.supports_service_tier(&service_tier)
    };
    config.service_tier = supported.then_some(service_tier);
    Ok(())
}

fn model_supports_selected_tier(
    config: &Config,
    info: &codex_protocol::openai_models::ModelInfo,
    tier: &str,
) -> bool {
    // The protocol helper treats Flex as universally available for OpenAI.
    // Other providers must advertise it in their own authoritative catalog.
    crate::session::model_supports_provider_service_tier(&config.model_provider_id, info, tier)
}

async fn apply_spawn_agent_role(
    models_manager: &SharedModelsManager,
    config: &mut Config,
    role: &PreparedAgentRole,
) -> Result<Option<String>, String> {
    let previous_session_layers = config
        .config_layer_stack
        .layers_low_to_high()
        .filter(|layer| layer.name == codex_config::ConfigLayerSource::SessionFlags)
        .count();
    let previous_model = config.model.clone();
    let previous_reasoning_effort = config.model_reasoning_effort.clone();
    role.apply(config)?;
    let explicit_tier = config
        .config_layer_stack
        .layers_low_to_high()
        .filter(|layer| layer.name == codex_config::ConfigLayerSource::SessionFlags)
        .nth(previous_session_layers)
        .and_then(|layer| layer.config.get("service_tier"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    if config.model == previous_model && config.model_reasoning_effort == previous_reasoning_effort
    {
        return Ok(explicit_tier);
    }

    let Some(reasoning_effort) = config.model_reasoning_effort.clone() else {
        return Ok(explicit_tier);
    };
    let model = config.model.clone().ok_or_else(|| {
        "spawn_agent could not resolve the child model for reasoning effort validation".to_string()
    })?;
    let model_info = models_manager
        .get_model_info(&model, &config.to_models_manager_config())
        .await;
    if model_info.used_fallback_model_metadata {
        return Ok(explicit_tier);
    }

    validate_spawn_agent_reasoning_effort(
        &model,
        &model_info.supported_reasoning_levels,
        &reasoning_effort,
    )?;
    Ok(explicit_tier)
}

fn find_spawn_agent_model_name(
    available_models: &[ModelPreset],
    requested_model: &str,
    multi_agent_version: MultiAgentVersion,
) -> Result<String, String> {
    available_models
        .iter()
        .find(|model| {
            model.model == requested_model
                && model_supports_multi_agent_backend(model, multi_agent_version)
        })
        .map(|model| model.model.clone())
        .ok_or_else(|| {
            let available = available_models
                .iter()
                .filter(|model| model.show_in_picker)
                .filter(|model| model_supports_multi_agent_backend(model, multi_agent_version))
                .take(MAX_SPAWN_AGENT_MODEL_OVERRIDES)
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Unknown model `{requested_model}` for spawn_agent. Available models: {available}"
            )
        })
}

fn validate_spawn_agent_reasoning_effort(
    model: &str,
    supported_reasoning_levels: &[ReasoningEffortPreset],
    requested_reasoning_effort: &ReasoningEffort,
) -> Result<(), String> {
    if supported_reasoning_levels
        .iter()
        .any(|preset| &preset.effort == requested_reasoning_effort)
    {
        return Ok(());
    }

    let supported = supported_reasoning_levels
        .iter()
        .map(|preset| preset.effort.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Reasoning effort `{requested_reasoning_effort}` is not supported for model `{model}`. Supported reasoning efforts: {supported}"
    ))
}

#[cfg(test)]
#[path = "child_config_tests.rs"]
mod tests;
