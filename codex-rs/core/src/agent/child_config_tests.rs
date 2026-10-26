use super::*;
use crate::config::ConfigBuilder;
use crate::config::ConfigOverrides;
use crate::thread_manager::StartThreadOptions;
use codex_config::child_provider::ChildProviderRequest;
use codex_protocol::openai_models::ModelsResponse;
use pretty_assertions::assert_eq;

fn prepared_source(
    parent: &crate::CodexThread,
    config: &Config,
) -> codex_protocol::protocol::SessionSource {
    codex_protocol::protocol::SessionSource::SubAgent(
        codex_protocol::protocol::SubAgentSource::ThreadSpawn {
            parent_thread_id: parent.session.thread_id,
            depth: crate::agent::next_thread_spawn_depth(&parent.session_source),
            agent_path: None,
            agent_nickname: None,
            agent_role: config
                .prepared_child_authorization
                .as_ref()
                .and_then(|proof| proof.role_name()),
        },
    )
}

async fn start_prepared_child(
    parent: &Arc<crate::CodexThread>,
    mut config: Config,
) -> Arc<crate::CodexThread> {
    let runtime = &parent.session.services.local_agent_runtime;
    let source = prepared_source(parent, &config);
    let authorization = config.prepared_child_authorization.take().unwrap();
    let permit = authorization
        .consume(runtime, parent.session.thread_id, &config, false, &source)
        .await
        .unwrap();
    runtime
        .upgrade()
        .unwrap()
        .spawn_child_thread(
            StartThreadOptions {
                session_source: Some(source),
                ..StartThreadOptions::new(config)
            },
            runtime.control(parent.session.services.agent_control.identity()),
            Some(parent.session.thread_id),
            None,
            permit,
        )
        .await
        .unwrap()
        .thread
}

struct Fixture {
    _host: tempfile::TempDir,
    _project: tempfile::TempDir,
    config: Config,
    manager: crate::ThreadManager,
    parent: Arc<crate::CodexThread>,
}

impl Fixture {
    async fn start_native_child(&self, config: Config) -> Arc<crate::CodexThread> {
        start_prepared_child(&self.parent, config).await
    }
    async fn new() -> Self {
        Self::with_fast_mode(None).await
    }

    async fn with_fast_mode(fast_mode: Option<bool>) -> Self {
        let host = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            host.path().join("config.toml"),
            r#"
[agents]
max_depth = 5
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
        let mut config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(host.path().to_path_buf())
            .harness_overrides(ConfigOverrides {
                cwd: Some(project.path().to_path_buf()),
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        if let Some(enabled) = fast_mode {
            if enabled {
                config.features.enable(Feature::FastMode).unwrap();
            } else {
                config.features.disable(Feature::FastMode).unwrap();
            }
        }
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
                model_provider: None,
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
    let child_a = fixture.start_native_child(zai).await;
    let child_b = fixture.start_native_child(ollama).await;
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
            model_provider: None,
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
    let role = prepare_role(&config, Some("tier")).await.unwrap();
    let explicit = apply_spawn_agent_role(&manager, &mut config, &role)
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
async fn advertised_custom_role_tier_requires_the_same_feature_gate_as_runtime() {
    for enabled in [false, true] {
        let fixture = Fixture::with_fast_mode(Some(enabled)).await;
        std::fs::write(
            fixture._host.path().join("tier.toml"),
            "service_tier = 'custom'\n",
        )
        .unwrap();
        let resolved = fixture.resolved("zai").await;
        let path = resolved.model_catalog_json.as_ref().unwrap();
        let mut catalog: ModelsResponse =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        catalog.models[0]
            .service_tiers
            .push(codex_protocol::openai_models::ModelServiceTier {
                id: "custom".into(),
                name: "Custom".into(),
                description: "fixture".into(),
            });
        std::fs::write(path, serde_json::to_vec(&catalog).unwrap()).unwrap();
        let result = fixture.prepared(&resolved, Some("tier"), None).await;
        if !enabled {
            assert!(
                result
                    .err()
                    .unwrap()
                    .contains("child_provider_capability_unsupported")
            );
        } else {
            let prepared = result.unwrap();
            assert_eq!(prepared.config.service_tier.as_deref(), Some("custom"));
            let child = fixture.start_native_child(prepared.config).await;
            let turn = child.session.new_default_turn().await;
            let step = child
                .session
                .capture_step_context(turn, &tokio_util::sync::CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(step.settings.service_tier.as_deref(), Some("custom"));
        }
    }
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
async fn native_step_does_not_reintroduce_an_unsupported_root_flex_tier() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture.prepared(&resolved, None, None).await.unwrap();
    let child = fixture.start_native_child(prepared.config).await;
    child
        .session
        .services
        .agent_control
        .propagate_config_update(crate::agent::api::AgentConfigUpdate::ServiceTier(Some(
            "flex".into(),
        )));
    let turn = child.session.new_default_turn().await;
    let step = child
        .session
        .capture_step_context(turn, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(step.settings.service_tier, None);
}

#[tokio::test]
async fn native_explicit_role_tier_survives_root_updates_and_is_not_inherited_as_role_authority() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let path = resolved.model_catalog_json.as_ref().unwrap();
    let mut catalog: ModelsResponse =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    catalog.models[0]
        .service_tiers
        .push(codex_protocol::openai_models::ModelServiceTier {
            id: "flex".into(),
            name: "Flex".into(),
            description: "fixture".into(),
        });
    std::fs::write(path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let prepared = fixture
        .prepared(&resolved, Some("tier"), None)
        .await
        .unwrap();
    assert!(prepared.config.child_role_service_tier_override);
    let child = fixture.start_native_child(prepared.config).await;
    for root_tier in [None, Some("default".into()), Some("unsupported".into())] {
        child
            .session
            .services
            .agent_control
            .propagate_config_update(crate::agent::api::AgentConfigUpdate::ServiceTier(root_tier));
        let turn = child.session.new_default_turn().await;
        assert!(
            !build_agent_resume_config(&turn)
                .unwrap()
                .child_role_service_tier_override
        );
        let step = child
            .session
            .capture_step_context(turn, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(step.settings.service_tier.as_deref(), Some("flex"));
    }
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
    let first = fixture.start_native_child(prepared.config).await;
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
            model_provider: None,
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
    let second = start_prepared_child(&first, prepared.config).await;
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

#[tokio::test]
async fn prepared_launch_rejects_whole_config_mutation_and_clone_replay() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, None, None)
        .await
        .unwrap()
        .config;
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    for mutation in 0..6 {
        let mut changed = prepared.clone();
        let authorization = changed.prepared_child_authorization.take().unwrap();
        match mutation {
            0 => changed.model_provider.base_url = Some("https://forged.invalid/v1".into()),
            1 => changed.model_catalog.as_mut().unwrap().models[0].context_window = Some(1),
            2 => changed.developer_instructions = Some("forged authority".into()),
            3 => changed.cwd = changed.codex_home.clone().try_into().unwrap(),
            4 => {
                changed
                    .set_legacy_sandbox_policy(
                        codex_protocol::protocol::SandboxPolicy::DangerFullAccess,
                    )
                    .unwrap();
            }
            _ => changed.model_context_window = Some(1),
        }
        assert!(
            authorization
                .consume(
                    runtime,
                    fixture.parent.session.thread_id,
                    &changed,
                    false,
                    &prepared_source(&fixture.parent, &fixture.config)
                )
                .await
                .is_err()
        );
    }
    let mut first = prepared.clone();
    let mut second = prepared;
    let permit = first
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            runtime,
            fixture.parent.session.thread_id,
            &first,
            false,
            &prepared_source(&fixture.parent, &fixture.config),
        )
        .await
        .unwrap();
    let replay = second
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            runtime,
            fixture.parent.session.thread_id,
            &second,
            false,
            &prepared_source(&fixture.parent, &fixture.config),
        )
        .await;
    assert!(replay.is_err_and(|error| error.to_string().contains("already consumed")));
    drop(permit);
}

#[tokio::test]
async fn prepared_launch_rejects_wrong_parent_tree_history_and_stale_host_settings() {
    let fixture = Fixture::new().await;
    let other = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, None, None)
        .await
        .unwrap()
        .config;
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    for (caller, tree, full_history) in [
        (codex_protocol::ThreadId::new(), runtime, false),
        (
            fixture.parent.session.thread_id,
            &other.parent.session.services.local_agent_runtime,
            false,
        ),
        (fixture.parent.session.thread_id, runtime, true),
    ] {
        let mut changed = prepared.clone();
        assert!(
            changed
                .prepared_child_authorization
                .take()
                .unwrap()
                .consume(
                    tree,
                    caller,
                    &changed,
                    full_history,
                    &prepared_source(&fixture.parent, &fixture.config)
                )
                .await
                .is_err()
        );
    }
    let current = fixture
        .parent
        .thread_settings_snapshot()
        .await
        .approval_policy;
    let replacement = if current == codex_protocol::protocol::AskForApproval::Never {
        codex_protocol::protocol::AskForApproval::OnRequest
    } else {
        codex_protocol::protocol::AskForApproval::Never
    };
    fixture
        .parent
        .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
            approval_policy: Some(replacement),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut changed = prepared;
    assert!(
        changed
            .prepared_child_authorization
            .take()
            .unwrap()
            .consume(
                runtime,
                fixture.parent.session.thread_id,
                &changed,
                false,
                &prepared_source(&fixture.parent, &fixture.config)
            )
            .await
            .is_err_and(|error| error.to_string().contains("authority changed"))
    );
}

#[tokio::test]
async fn public_root_start_cannot_forge_a_delegated_source_or_reuse_prepared_config() {
    let fixture = Fixture::new().await;
    let source = codex_protocol::protocol::SessionSource::SubAgent(
        codex_protocol::protocol::SubAgentSource::ThreadSpawn {
            parent_thread_id: fixture.parent.session.thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        },
    );
    let forged = fixture
        .manager
        .start_thread(StartThreadOptions {
            session_source: Some(source),
            ..StartThreadOptions::new(fixture.config.clone())
        })
        .await;
    assert!(forged.is_err_and(|error| error.to_string().contains("private native launch path")));
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, None, None)
        .await
        .unwrap()
        .config;
    assert!(
        fixture
            .manager
            .start_thread(StartThreadOptions::new(prepared))
            .await
            .is_err_and(|error| error.to_string().contains("independent root"))
    );
}

#[tokio::test]
async fn prepared_role_reads_once_and_ignores_later_role_replacement() {
    let fixture = Fixture::new().await;
    let role = crate::agent::role::prepare_role(&fixture.config, Some("reasoning"))
        .await
        .unwrap();
    std::fs::write(
        fixture._host.path().join("reasoning.toml"),
        "model_provider = 'ollama'\nmodel = 'hostile-new-model'\nmodel_reasoning_effort = 'high'\n",
    )
    .unwrap();
    let mut config = fixture.config.clone();
    let provider = config.model_provider.clone();
    role.apply(&mut config).unwrap();
    assert_eq!(config.model_provider, provider);
    assert_eq!(config.agent_role_model_provider, None);
    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::Low));
    assert_ne!(config.model.as_deref(), Some("hostile-new-model"));
}

#[tokio::test]
async fn direct_controller_cannot_start_forged_config_or_run_its_auth_helper() {
    let fixture = Fixture::new().await;
    let mut config = fixture.config.clone();
    let script = fixture._host.path().join(if cfg!(windows) {
        "forged.cmd"
    } else {
        "forged.sh"
    });
    std::fs::write(
        &script,
        if cfg!(windows) {
            "@echo off\r\necho invoked>forged-helper-marker\r\necho synthetic-forged\r\n"
        } else {
            "touch forged-helper-marker\nprintf synthetic-forged\n"
        },
    )
    .unwrap();
    let auth = if cfg!(windows) {
        serde_json::json!({"command":"cmd.exe","args":["/D","/C",script],"cwd":fixture._host.path(),"timeout_ms":5000,"refresh_interval_ms":0})
    } else {
        serde_json::json!({"command":"sh","args":[script],"cwd":fixture._host.path(),"timeout_ms":5000,"refresh_interval_ms":0})
    };
    config.model_provider.auth = Some(serde_json::from_value(auth).unwrap());
    let caller = fixture.parent.session.thread_id;
    let result = fixture
        .parent
        .session
        .services
        .agent_control
        .spawn(crate::agent::api::SpawnRequest {
            caller,
            config,
            input: crate::agent::api::AgentInput::UserInput(vec![
                codex_protocol::user_input::UserInput::Text {
                    text: "forged".into(),
                    text_elements: vec![],
                },
            ]),
            source: codex_protocol::protocol::SessionSource::SubAgent(
                codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                    parent_thread_id: caller,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                },
            ),
            options: crate::agent::types::SpawnAgentOptions {
                parent_thread_id: Some(caller),
                ..Default::default()
            },
        })
        .await;
    assert!(result.is_err_and(|error| error.to_string().contains("host-prepared authorization")));
    assert!(!fixture._host.path().join("forged-helper-marker").exists());
    assert_eq!(fixture.manager.list_thread_ids().await.len(), 1);
}

#[tokio::test]
async fn runtime_permission_change_rechecks_host_catalog_and_definition_provenance() {
    let fixture = Fixture::new().await;
    fixture
        .parent
        .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
            permission_profile: Some(codex_protocol::models::PermissionProfile::Disabled),
            ..Default::default()
        })
        .await
        .unwrap();
    let resolved = fixture.resolved("zai").await;
    let result = fixture.prepared(&resolved, None, None).await;
    assert!(
        result.is_err_and(|error| error.contains("child_provider_untrusted_source")
            || error.contains("child_provider_not_authorized")),
        "a writable host definition/catalog cannot retain its old trust exemption"
    );
    assert_eq!(fixture.manager.list_thread_ids().await.len(), 1);
}
