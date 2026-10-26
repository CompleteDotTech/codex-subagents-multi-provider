use super::*;
use crate::config::ConfigBuilder;
use crate::config::ConfigOverrides;
use crate::thread_manager::StartThreadOptions;
use codex_config::child_provider::ChildProviderRequest;
use codex_protocol::openai_models::ModelsResponse;
use pretty_assertions::assert_eq;

struct Fixture {
    _host: tempfile::TempDir,
    _project: tempfile::TempDir,
    config: Config,
    manager: crate::ThreadManager,
    parent: Arc<crate::CodexThread>,
}

impl Fixture {
    async fn new() -> Self {
        let host = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            host.path().join("config.toml"),
            r#"
[agents]
allowed_model_providers = ["zai", "ollama", "openai", "offline"]
[agents.provider_defaults.zai]
model = "shared"
model_catalog_json = "zai.json"
[agents.provider_defaults.offline]
model = "gpt-5.5"
[agents.provider_defaults.ollama]
model = "shared"
model_catalog_json = "ollama.json"
[agents.tier]
description = "Select an explicit service tier"
config_file = "tier.toml"
[agents.reasoning]
description = "Override reasoning effort"
config_file = "reasoning.toml"
[agents.default_tier]
description = "Use the default tier sentinel"
config_file = "default-tier.toml"
[model_providers.zai]
name = "Fixture Z.ai"
base_url = "http://127.0.0.1:1/v1"
[model_providers.offline]
name = "Unavailable authoritative catalog"
base_url = "http://127.0.0.1:1/v1"
model_catalog_url = "http://127.0.0.1:1/catalog"
request_max_retries = 0
"#,
        )
        .unwrap();
        std::fs::write(host.path().join("tier.toml"), "service_tier = 'flex'\n").unwrap();
        std::fs::write(
            host.path().join("default-tier.toml"),
            "service_tier = 'default'\n",
        )
        .unwrap();
        std::fs::write(
            host.path().join("reasoning.toml"),
            "model_reasoning_effort = 'low'\n",
        )
        .unwrap();
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(host.path().to_path_buf())
            .harness_overrides(ConfigOverrides {
                cwd: Some(project.path().to_path_buf()),
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        for role in ["tier", "reasoning", "default_tier"] {
            assert!(
                config.agent_roles.contains_key(role),
                "fixture role `{role}` was not loaded: {:?}",
                config.startup_warnings
            );
        }
        let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
            codex_login::CodexAuth::from_api_key("parent-only"),
            config.model_provider.clone(),
            host.path().to_path_buf(),
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        );
        let parent = manager
            .start_thread(StartThreadOptions::new(config.clone()))
            .await
            .unwrap()
            .thread;
        let parent_info = parent
            .session
            .new_default_turn()
            .await
            .model_info()
            .as_ref()
            .clone();
        for (provider, window, effort) in [
            ("zai", 32_000, ReasoningEffort::Low),
            ("ollama", 8_000, ReasoningEffort::Medium),
        ] {
            let mut info = parent_info.clone();
            info.slug = "shared".into();
            info.context_window = Some(window);
            info.max_context_window = Some(window);
            info.default_reasoning_level = Some(effort.clone());
            info.supported_reasoning_levels = vec![ReasoningEffortPreset {
                effort,
                description: "fixture".into(),
            }];
            info.service_tiers.clear();
            info.additional_speed_tiers.clear();
            info.default_service_tier = None;
            info.used_fallback_model_metadata = false;
            std::fs::write(
                host.path().join(format!("{provider}.json")),
                serde_json::to_vec(&ModelsResponse { models: vec![info] }).unwrap(),
            )
            .unwrap();
        }
        Self {
            _host: host,
            _project: project,
            config,
            manager,
            parent,
        }
    }

    async fn resolved(&self, provider: &str) -> ResolvedChildProvider {
        let turn = self.parent.session.new_default_turn().await;
        self.config
            .child_provider_policy
            .resolve(ChildProviderRequest {
                explicit_provider: Some(provider),
                role_provider: None,
                parent_provider_id: &self.config.model_provider_id,
                parent_provider: &self.config.model_provider,
                parent_model: &turn.model_info().slug,
                authorization_origin: None,
                explicit_model: None,
                role_model: None,
                default_subagent_model: None,
                full_history: false,
                host_owned: false,
            })
            .unwrap()
    }

    async fn prepared(
        &self,
        resolved: &ResolvedChildProvider,
        role: Option<&str>,
        effort: Option<ReasoningEffort>,
    ) -> Result<PreparedSpawnConfig, String> {
        let turn = self.parent.session.new_default_turn().await;
        let step = StepContext::for_test(turn);
        prepare_agent_spawn_config(
            &self.parent.session,
            &step,
            SpawnConfigOptions {
                resolved_provider: Some(resolved),
                version: SpawnConfigVersion::V2,
                full_history_fork: false,
                role_name: role,
                model: None,
                reasoning_effort: effort,
            },
        )
        .await
    }
}

#[tokio::test]
async fn selected_same_slug_catalog_reaches_startup_without_changing_parent_or_sibling() {
    let fixture = Fixture::new().await;
    let parent_turn = fixture.parent.session.new_default_turn().await;
    let parent_info = parent_turn.model_info().clone();
    let zai = fixture.resolved("zai").await;
    let ollama = fixture.resolved("ollama").await;
    let (zai, ollama) = tokio::join!(
        fixture.prepared(&zai, None, None),
        fixture.prepared(&ollama, None, None)
    );
    let zai = zai.unwrap().config;
    let ollama = ollama.unwrap().config;
    assert_eq!(zai.model_reasoning_effort, Some(ReasoningEffort::Low));
    assert_eq!(ollama.model_reasoning_effort, Some(ReasoningEffort::Medium));
    assert_eq!(zai.model_context_window, None);
    assert_eq!(zai.service_tier, None);
    // Actual thread startup must consume this manager instead of the root's manager.
    let child_a = fixture
        .manager
        .start_thread(StartThreadOptions::new(zai))
        .await
        .unwrap()
        .thread;
    let child_b = fixture
        .manager
        .start_thread(StartThreadOptions::new(ollama))
        .await
        .unwrap()
        .thread;
    let turn_a = child_a.session.new_default_turn().await;
    let turn_b = child_b.session.new_default_turn().await;
    assert_eq!(turn_a.model_info().slug, "shared");
    assert_eq!(turn_a.model_info().context_window, Some(32_000));
    assert_eq!(turn_b.model_info().context_window, Some(8_000));
    assert!(turn_a.config.prepared_child_models.is_none());
    assert!(turn_b.config.prepared_child_models.is_none());
    assert!(
        build_agent_resume_config(&turn_a)
            .unwrap()
            .prepared_child_models
            .is_none()
    );
    let sibling = fixture
        .manager
        .start_thread(StartThreadOptions::new(fixture.config.clone()))
        .await
        .unwrap()
        .thread;
    assert_eq!(
        fixture.parent.session.new_default_turn().await.model_info(),
        &parent_info
    );
    assert_eq!(
        sibling.session.new_default_turn().await.model_info(),
        &parent_info
    );
}

#[tokio::test]
async fn prepared_scope_cannot_survive_provider_catalog_model_or_auth_changes() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, None, None)
        .await
        .unwrap()
        .config;
    for mutation in 0..5 {
        let mut config = prepared.clone();
        match mutation {
            0 => config.model_provider_id = "ollama".into(),
            1 => config.model_provider.base_url = Some("https://changed.invalid/v1".into()),
            2 => config.model_catalog.as_mut().unwrap().models[0].context_window = Some(1),
            3 => config.model = Some("changed".into()),
            _ => {}
        }
        let auth = if mutation == 4 {
            codex_login::AuthManager::from_auth_for_testing(codex_login::CodexAuth::from_api_key(
                "other",
            ))
        } else {
            Arc::clone(&fixture.parent.session.services.auth_manager)
        };
        assert!(
            take_prepared_child_models(&mut config, &auth)
                .unwrap_err()
                .contains("scope changed")
        );
        assert!(config.prepared_child_models.is_none());
    }
}

#[tokio::test]
async fn unknown_model_and_unsupported_explicit_effort_fail_without_parent_fallback() {
    let fixture = Fixture::new().await;
    let mut resolved = fixture.resolved("zai").await;
    let result = fixture
        .prepared(&resolved, None, Some(ReasoningEffort::High))
        .await;
    assert!(
        result
            .err()
            .unwrap()
            .contains("child_provider_capability_unsupported")
    );
    resolved.model = "gpt-5.5".into();
    let result = fixture.prepared(&resolved, None, None).await;
    assert!(
        result
            .err()
            .unwrap()
            .contains("child_provider_model_unavailable")
    );
}

#[tokio::test]
async fn failed_target_catalog_discovery_cannot_validate_a_bundled_openai_slug() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("offline").await;
    let result = fixture.prepared(&resolved, None, None).await;
    assert!(
        result
            .err()
            .unwrap()
            .contains("child_provider_model_unavailable")
    );
}

#[tokio::test]
async fn prepared_provider_selection_cannot_be_reused_for_a_full_history_switch() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    std::fs::remove_file(resolved.model_catalog_json.as_ref().unwrap()).unwrap();
    let turn = fixture.parent.session.new_default_turn().await;
    let step = StepContext::for_test(turn);
    let result = prepare_agent_spawn_config(
        &fixture.parent.session,
        &step,
        SpawnConfigOptions {
            resolved_provider: Some(&resolved),
            version: SpawnConfigVersion::V2,
            full_history_fork: true,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await;
    assert!(
        result
            .err()
            .unwrap()
            .contains("child_provider_history_unsupported")
    );
}

#[tokio::test]
async fn explicit_role_tier_is_validated_even_when_equal_to_inherited_value() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let result = fixture.prepared(&resolved, Some("tier"), None).await;
    assert!(
        result
            .err()
            .unwrap()
            .contains("child_provider_capability_unsupported")
    );
    let mut config = fixture
        .prepared(&resolved, None, None)
        .await
        .unwrap()
        .config;
    config.service_tier = Some("flex".into());
    let manager = config
        .prepared_child_models
        .as_ref()
        .unwrap()
        .manager
        .clone();
    let explicit = apply_spawn_agent_role(&manager, &mut config, Some("tier"))
        .await
        .unwrap();
    assert_eq!(explicit, Some("flex".into()));
    let default = fixture
        .prepared(&resolved, Some("default_tier"), None)
        .await
        .unwrap();
    assert_eq!(
        default.config.service_tier,
        Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.into())
    );
}

#[tokio::test]
async fn unsupported_inherited_tier_is_omitted_and_default_sentinel_is_preserved() {
    let fixture = Fixture::new().await;
    fixture
        .parent
        .session
        .services
        .agent_control
        .propagate_config_update(crate::agent::api::AgentConfigUpdate::ServiceTier(Some(
            "flex".into(),
        )));
    let resolved = fixture.resolved("zai").await;
    assert_eq!(
        fixture
            .prepared(&resolved, None, None)
            .await
            .unwrap()
            .config
            .service_tier,
        None
    );
    fixture
        .parent
        .session
        .services
        .agent_control
        .propagate_config_update(crate::agent::api::AgentConfigUpdate::ServiceTier(Some(
            SERVICE_TIER_DEFAULT_REQUEST_VALUE.into(),
        )));
    assert_eq!(
        fixture
            .prepared(&resolved, None, None)
            .await
            .unwrap()
            .config
            .service_tier,
        Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.into())
    );
}

#[tokio::test]
async fn changed_provider_uses_target_reasoning_default_and_preserves_role_precedence() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let mut config = fixture.config.clone();
    config.agent_default_subagent_reasoning_effort = Some(ReasoningEffort::High);
    prepare_resolved_child_models(&fixture.parent.session, &mut config, &resolved, None, None)
        .await
        .unwrap();
    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::Low));
    let prepared = fixture
        .prepared(&resolved, Some("reasoning"), Some(ReasoningEffort::High))
        .await
        .unwrap();
    assert_eq!(
        prepared.config.model_reasoning_effort,
        Some(ReasoningEffort::Low)
    );
}

#[tokio::test]
async fn fresh_same_provider_reload_uses_changed_catalog_prompts_and_limits() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture.prepared(&resolved, None, None).await.unwrap();
    let first = fixture
        .manager
        .start_thread(StartThreadOptions::new(prepared.config))
        .await
        .unwrap()
        .thread;
    let turn = first.session.new_default_turn().await;
    let mut info = turn.model_info().as_ref().clone();
    info.context_window = Some(4_000);
    info.max_context_window = Some(4_000);
    info.model_messages.as_mut().unwrap().instructions_template =
        Some("fresh target model instructions".into());
    std::fs::write(
        resolved.model_catalog_json.as_ref().unwrap(),
        serde_json::to_vec(&ModelsResponse { models: vec![info] }).unwrap(),
    )
    .unwrap();
    // Resolve again using the switched immediate parent's actual identity. No
    // authorization origin is invented; current allowlist membership authorizes it.
    let next = fixture
        .config
        .child_provider_policy
        .resolve(ChildProviderRequest {
            explicit_provider: None,
            role_provider: None,
            parent_provider_id: "zai",
            parent_provider: &resolved.provider,
            parent_model: "shared",
            authorization_origin: None,
            explicit_model: None,
            role_model: None,
            default_subagent_model: None,
            full_history: false,
            host_owned: false,
        })
        .unwrap();
    assert!(!next.changed_provider);
    let step = StepContext::for_test(turn);
    let prepared = prepare_agent_spawn_config(
        &first.session,
        &step,
        SpawnConfigOptions {
            resolved_provider: Some(&next),
            version: SpawnConfigVersion::V2,
            full_history_fork: false,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .unwrap();
    let second = fixture
        .manager
        .start_thread(StartThreadOptions::new(prepared.config))
        .await
        .unwrap()
        .thread;
    assert_eq!(
        second
            .session
            .new_default_turn()
            .await
            .model_info()
            .context_window,
        Some(4_000)
    );
    assert!(
        second
            .session
            .get_base_instructions()
            .await
            .text
            .contains("fresh target model instructions")
    );
    assert_eq!(
        first
            .session
            .new_default_turn()
            .await
            .model_info()
            .context_window,
        Some(32_000)
    );
}
