use super::*;
use codex_config::RequirementSource;
use codex_config::Sourced;
use codex_config::config_toml::ForcedChatgptWorkspaceIds;
use codex_config::types::AuthCredentialsStoreMode;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_features::Features;
use codex_protocol::config_types::ForcedLoginMethod;
use pretty_assertions::assert_eq;

fn options(home: &std::path::Path, cwd: &std::path::Path) -> EmbeddedHostConfigOptions {
    EmbeddedHostConfigOptions {
        codex_home: AbsolutePathBuf::from_absolute_path(home).expect("absolute home"),
        cwd: AbsolutePathBuf::from_absolute_path(cwd).expect("absolute cwd"),
        model: Some("host-model".to_string()),
        codex_self_exe: Some(cwd.join("host-codex")),
        codex_linux_sandbox_exe: Some(cwd.join("host-sandbox")),
    }
}

#[tokio::test]
async fn bootstrap_execution_layers_are_not_imported() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let cwd = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.toml"),
        r#"
model = "foreign-model"
model_provider = "foreign"
openai_base_url = "https://foreign.invalid/v1"
approval_policy = "never"
sandbox_mode = "danger-full-access"
instructions = "foreign instructions"
developer_instructions = "foreign developer instructions"
notify = ["foreign-program"]
web_search = "live"
cli_auth_credentials_store = "keyring"
mcp_oauth_credentials_store = "auto"
[tools.update_plan]
enabled = false
[model_providers.foreign]
name = "Foreign"
base_url = "https://foreign.invalid/v1"
wire_api = "responses"
[mcp_servers.foreign]
command = "foreign-program"
[plugins."foreign@test"]
enabled = true
[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "foreign-hook-program"
[features]
secret_auth_storage = true
respect_system_proxy = true
"#,
    )?;
    let cwd_abs = AbsolutePathBuf::from_absolute_path(cwd.path())?;
    let bootstrap = super::super::load_config_toml_with_layer_stack(
        home.path(),
        Some(&cwd_abs),
        Vec::new(),
        super::super::ConfigLoadOptions::default(),
    )
    .await?;
    assert!(
        bootstrap
            .config_layer_stack
            .layers_low_to_high()
            .next()
            .is_some()
    );
    let config = Config::for_embedded_host(options(home.path(), cwd.path()), &bootstrap).await?;
    assert_eq!(config.model.as_deref(), Some("host-model"));
    assert_eq!(config.model_provider_id, OPENAI_PROVIDER_ID);
    assert!(!config.model_providers.contains_key("foreign"));
    assert_eq!(config.model_provider.base_url, None);
    assert_eq!(
        config.permissions.approval_policy.value(),
        AskForApproval::Never
    );
    assert_eq!(
        config.permissions.permission_profile(),
        &PermissionProfile::read_only()
    );
    assert_eq!(config.web_search_mode.value(), WebSearchMode::Disabled);
    assert!(config.ephemeral);
    assert!(
        config
            .config_layer_stack
            .layers_low_to_high()
            .next()
            .is_none()
    );
    assert!(config.mcp_servers.get().is_empty());
    assert!(config.agent_roles.is_empty());
    assert!(config.plugins.plugins.is_empty());
    assert!(config.respect_system_proxy);
    assert!(config.base_instructions.is_none());
    assert!(config.developer_instructions.is_none());
    assert!(config.notify.is_none());
    assert!(config.prepared_child_models.is_none());
    assert!(!config.child_role_service_tier_override);
    assert_eq!(
        config.cli_auth_credentials_store_mode,
        AuthCredentialsStoreMode::Keyring
    );
    assert_eq!(
        config.mcp_oauth_credentials_store_mode,
        OAuthCredentialsStoreMode::Auto
    );
    assert!(config.features.enabled(Feature::SecretAuthStorage));
    let mut expected_features = Features::with_defaults();
    expected_features.set_enabled(Feature::SecretAuthStorage, /*enabled*/ true);
    assert_eq!(config.features.get(), &expected_features);
    assert_eq!(config.sqlite.home(), home.path());
    assert_eq!(config.log_dir, home.path().join("log"));
    assert_eq!(
        config.sqlite.logs_db_path(),
        codex_state::SqliteConfig::from_sqlite_home(config.codex_home.clone()).logs_db_path()
    );
    assert!(config.update_plan_enabled);
    assert!(!config.include_permissions_instructions);
    assert!(!config.include_skill_instructions);
    assert!(!config.cloud_skill_enabled);
    assert!(!config.orchestrator_mcp_enabled);
    assert_eq!(config.agent_max_threads, Some(6));
    assert_eq!(config.agent_max_depth, 1);
    assert_eq!(config.analytics_enabled, Some(false));
    assert!(!config.feedback_enabled);
    assert_eq!(config.codex_self_exe, Some(cwd.path().join("host-codex")));
    Ok(())
}

#[tokio::test]
async fn managed_auth_survives_rebuilding_from_embedded_config() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let cwd = tempfile::tempdir()?;
    for store in [
        AuthCredentialsStoreMode::Auto,
        AuthCredentialsStoreMode::Keyring,
    ] {
        let requirements = ConfigRequirements {
            allowed_login_methods: Some(Sourced::new(
                vec![ForcedLoginMethod::Chatgpt],
                RequirementSource::Unknown,
            )),
            allowed_chatgpt_workspaces: Some(Sourced::new(
                vec!["allowed".to_string()],
                RequirementSource::Unknown,
            )),
            cli_auth_credentials_store: Some(Sourced::new(store, RequirementSource::Unknown)),
            chatgpt_base_url: Some(Sourced::new(
                "https://managed.invalid/backend-api/".to_string(),
                RequirementSource::Unknown,
            )),
            ..Default::default()
        };
        let bootstrap = ConfigTomlLoadResult {
            config_toml: ConfigToml {
                cli_auth_credentials_store: Some(AuthCredentialsStoreMode::File),
                forced_chatgpt_workspace_id: Some(ForcedChatgptWorkspaceIds::Multiple(vec![
                    "denied".to_string(),
                    " allowed ".to_string(),
                ])),
                ..Default::default()
            },
            config_layer_stack: ConfigLayerStack::new(
                Vec::new(),
                requirements,
                ConfigRequirementsToml::default(),
            )?,
        };
        let expected = bootstrap_auth_config(home.path(), &bootstrap)?;
        let config =
            Config::for_embedded_host(options(home.path(), cwd.path()), &bootstrap).await?;
        let actual = config.auth_config();
        assert_eq!(actual.auth_credentials_store_mode, store);
        assert_eq!(actual.chatgpt_base_url, expected.chatgpt_base_url);
        assert_eq!(
            actual.effective_chatgpt_workspaces(),
            Some(vec!["allowed".to_string()])
        );
        assert!(actual.is_login_method_allowed(ForcedLoginMethod::Chatgpt));
        assert!(!actual.is_login_method_allowed(ForcedLoginMethod::Api));
        assert_eq!(actual.keyring_backend_kind, expected.keyring_backend_kind);
        assert_eq!(
            config.features.enabled(Feature::SecretAuthStorage),
            Features::with_defaults().enabled(Feature::SecretAuthStorage)
        );
    }
    Ok(())
}

#[tokio::test]
async fn invalid_bootstrap_auth_is_rejected_before_host_construction() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let cwd = tempfile::tempdir()?;
    let bootstrap = ConfigTomlLoadResult {
        config_toml: ConfigToml::default(),
        config_layer_stack: ConfigLayerStack::new(
            Vec::new(),
            ConfigRequirements {
                allowed_login_methods: Some(Sourced::new(
                    vec![ForcedLoginMethod::Chatgpt],
                    RequirementSource::Unknown,
                )),
                allowed_chatgpt_workspaces: Some(Sourced::new(
                    Vec::new(),
                    RequirementSource::Unknown,
                )),
                ..Default::default()
            },
            ConfigRequirementsToml::default(),
        )?,
    };
    let error = Config::for_embedded_host(options(home.path(), cwd.path()), &bootstrap)
        .await
        .expect_err("invalid managed authentication must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    Ok(())
}
