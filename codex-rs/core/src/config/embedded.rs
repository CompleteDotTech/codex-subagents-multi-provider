use super::Config;
use super::ConfigOverrides;
use super::ConfigTomlLoadResult;
use super::LOCAL_FS;
use super::bootstrap_auth_config;
use super::resolve_bootstrap_respect_system_proxy;
use codex_config::ConfigLayerStack;
use codex_config::ConfigRequirements;
use codex_config::ConfigRequirementsToml;
use codex_config::config_toml::ConfigToml;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::SessionPickerViewMode;
use codex_features::Feature;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::PathBuf;

/// Host paths and model for an isolated, ephemeral OpenAI embedding.
///
/// This supplies no delegated-child authority or prepared provider state.
pub struct EmbeddedHostConfigOptions {
    pub codex_home: AbsolutePathBuf,
    pub cwd: AbsolutePathBuf,
    pub model: Option<String>,
    pub codex_self_exe: Option<PathBuf>,
    pub codex_linux_sandbox_exe: Option<PathBuf>,
}

impl Config {
    /// Construct the sample host's read-only/Never configuration through the
    /// canonical initializer, retaining only authentication from the bootstrap.
    ///
    /// Unlike the normal configuration builder, this intentionally does not
    /// import bootstrap execution settings, providers, roles, plugins or hooks.
    /// The bootstrap's validated authentication restrictions remain applicable
    /// to every authentication manager subsequently built from this config.
    pub async fn for_embedded_host(
        options: EmbeddedHostConfigOptions,
        auth_bootstrap: &ConfigTomlLoadResult,
    ) -> std::io::Result<Self> {
        let auth = bootstrap_auth_config(options.codex_home.as_path(), auth_bootstrap)?;
        let requirements = auth_bootstrap.config_layer_stack.requirements();
        let requirements_toml = auth_bootstrap.config_layer_stack.requirements_toml();
        let stack = ConfigLayerStack::new(
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
        let cfg = ConfigToml {
            // Explicit state roots prevent ambient CODEX_SQLITE_HOME selection.
            sqlite_home: Some(options.codex_home.clone()),
            mcp_optional_startup_grace_ms: Some(1000),
            include_permissions_instructions: Some(false),
            include_apps_instructions: Some(false),
            include_collaboration_mode_instructions: Some(false),
            include_environment_context: Some(false),
            web_search: Some(WebSearchMode::Disabled),
            ..Default::default()
        };
        let overrides = ConfigOverrides {
            model: options.model,
            cwd: Some(options.cwd.to_path_buf()),
            model_provider: Some(OPENAI_PROVIDER_ID.to_string()),
            approval_policy: Some(AskForApproval::Never),
            permission_profile: Some(PermissionProfile::read_only()),
            ephemeral: Some(true),
            codex_self_exe: options.codex_self_exe,
            codex_linux_sandbox_exe: options.codex_linux_sandbox_exe,
            ..Default::default()
        };
        let mut config = Self::load_config_with_layer_stack(
            LOCAL_FS.as_ref(),
            cfg,
            overrides,
            options.codex_home,
            stack,
        )
        .await?;
        // Preserve the sample's explicit deviations from full-loader defaults.
        config.include_skill_instructions = false;
        config.cloud_skill_enabled = false;
        config.orchestrator_mcp_enabled = false;
        config.agent_max_threads = Some(6);
        config.agent_max_depth = 1;
        config.agent_interrupt_message_enabled = false;
        config.tui_fullscreen_transcript = false;
        config.tui_session_picker_view = SessionPickerViewMode::Dense;
        config.check_for_update_on_startup = false;
        config.analytics_enabled = Some(false);
        config.feedback_enabled = false;
        config.update_plan_enabled = true;
        // The ordinary loader coerces secure stores to File for version0.0.0.
        // Embedded cargo-run hosts retain their validated bootstrap selection.
        config.cli_auth_credentials_store_mode = auth.auth_credentials_store_mode;
        config.mcp_oauth_credentials_store_mode = auth_bootstrap
            .config_toml
            .mcp_oauth_credentials_store
            .unwrap_or_default();
        config.application_auth_route_config = Some(auth.auth_route_config.clone());
        config.forced_login_method = auth.forced_login_method;
        config.forced_chatgpt_workspace_id = auth.forced_chatgpt_workspace_id;
        config.chatgpt_base_url = auth
            .chatgpt_base_url
            .unwrap_or_else(|| "https://chatgpt.com/backend-api/".to_string());
        config.respect_system_proxy = resolve_bootstrap_respect_system_proxy(
            &auth_bootstrap.config_toml,
            requirements.feature_requirements.as_ref(),
        )?;
        config.features.set_enabled(
            Feature::SecretAuthStorage,
            auth.keyring_backend_kind == AuthKeyringBackendKind::Secrets,
        )?;
        Ok(config)
    }
}

#[cfg(test)]
#[path = "embedded_tests.rs"]
mod tests;
